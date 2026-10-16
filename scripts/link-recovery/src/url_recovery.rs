//! Target-free failure policy and one-shot ownership for link recovery.
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OpenFailure {
    InvalidUri,
    MissingHandler,
    PermissionDenied,
    Failed,
}

impl OpenFailure {
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::InvalidUri => "This link is invalid or unsupported.",
            Self::MissingHandler => "No browser is available to open this link.",
            Self::PermissionDenied => "Opening this link was denied.",
            Self::Failed => "The link could not be opened.",
        }
    }
}

/// Validate before invoking even an asynchronous dispatcher. The original
/// bytes are borrowed unchanged; validation failures never reach the backend.
pub(super) fn dispatch_with(uri: &str, dispatcher: impl FnOnce(&str)) -> Result<(), OpenFailure> {
    if !jterm_core::link::is_openable_url(uri) {
        return Err(OpenFailure::InvalidUri);
    }
    dispatcher(uri);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LaunchOutcome {
    Opened,
    Cancelled,
    Failed(OpenFailure),
}

/// Display-only origin derived from the same validated target; never use this
/// summary as the launch/copy target. Core rejects ambiguous authorities.
pub(super) fn safe_origin(uri: &str) -> Option<String> {
    if !jterm_core::link::is_openable_url(uri) {
        return None;
    }
    let (scheme, rest) = uri.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']')?;
        let host = format!("[{}]", host.parse::<std::net::Ipv6Addr>().ok()?);
        (host, suffix.strip_prefix(':'))
    } else {
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, None), |(h, p)| (h, Some(p)));
        (host.to_ascii_lowercase(), port)
    };
    let port = match port {
        Some(port) => format!(":{}", port.parse::<u16>().ok()?),
        None => String::new(),
    };
    Some(format!("{}://{host}{port}", scheme.to_ascii_lowercase()))
}

/// Identity, rather than a wrapping counter, rejects stale and repeated actions.
#[derive(Default)]
pub(super) struct RecoveryOwner(Option<Rc<()>>);

impl RecoveryOwner {
    pub(super) fn begin(&mut self) -> Rc<()> {
        let ticket = Rc::new(());
        self.0 = Some(ticket.clone());
        ticket
    }

    pub(super) fn clear(&mut self) {
        self.0 = None;
    }

    /// Returns whether this completion owned the request, and whether it may
    /// show a failure. Stale callbacks cannot clear a newer cancellable/prompt.
    pub(super) fn complete_launch(
        &mut self,
        ticket: &Rc<()>,
        mapped: bool,
        outcome: LaunchOutcome,
    ) -> (bool, Option<OpenFailure>) {
        if !self.consume(ticket) {
            return (false, None);
        }
        let failure = match outcome {
            LaunchOutcome::Failed(failure) if mapped => Some(failure),
            _ => None,
        };
        (true, failure)
    }

    pub(super) fn consume(&mut self, ticket: &Rc<()>) -> bool {
        if self
            .0
            .as_ref()
            .is_some_and(|current| Rc::ptr_eq(current, ticket))
        {
            self.clear();
            true
        } else {
            false
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RecoveryAction<'a> {
    None,
    Retry(&'a str),
    Copy(&'a str),
}

pub(super) fn recovery_action<'a>(
    response: Option<i32>,
    recoverable: bool,
    original: &'a str,
) -> RecoveryAction<'a> {
    if !recoverable || !jterm_core::link::is_openable_url(original) {
        return RecoveryAction::None;
    }
    match response {
        Some(1) => RecoveryAction::Retry(original),
        Some(2) => RecoveryAction::Copy(original),
        _ => RecoveryAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_fixed_and_never_contain_the_target() {
        let uri = "https://example.com/private-secret?token=query-secret#fragment-secret";
        for failure in [
            OpenFailure::MissingHandler,
            OpenFailure::PermissionDenied,
            OpenFailure::InvalidUri,
            OpenFailure::Failed,
        ] {
            let mut calls = 0;
            let mut owner = RecoveryOwner::default();
            let ticket = owner.begin();
            assert_eq!(
                dispatch_with(uri, |target| {
                    calls += 1;
                    assert_eq!(target, uri);
                }),
                Ok(())
            );
            let result = owner.complete_launch(&ticket, true, LaunchOutcome::Failed(failure));
            assert_eq!(result, (true, Some(failure)));
            assert_eq!(calls, 1, "no implicit retry");
            let rendered = format!("{:?}: {}", result, failure.message());
            for secret in [
                "example.com",
                "private-secret",
                "query-secret",
                "fragment-secret",
            ] {
                assert!(!rendered.contains(secret));
            }
        }
    }

    #[test]
    fn refused_targets_never_reach_an_opener() {
        for uri in [
            "file:///etc/passwd",
            "ssh://example.com",
            "https://user:secret@example.com",
            "https:///host",
            "https://%65xample.com/secret",
            "https://example.com/\nsecret",
            "https://example.com/\u{202e}secret",
        ] {
            assert_eq!(
                dispatch_with(uri, |_| panic!("must not launch")),
                Err(OpenFailure::InvalidUri)
            );
            assert_eq!(safe_origin(uri), None);
        }
    }

    #[test]
    fn origin_excludes_all_target_secrets_and_preserves_explicit_port() {
        for (uri, origin) in [
            (
                "HTTPS://Example.COM:00443/private?token=secret#fragment",
                "https://example.com:443",
            ),
            (
                "https://[2001:0db8::1]:8443/private",
                "https://[2001:db8::1]:8443",
            ),
            ("http://127.0.0.1:8080/?secret", "http://127.0.0.1:8080"),
            (
                "https://xn--bcher-kva.example/secret",
                "https://xn--bcher-kva.example",
            ),
        ] {
            assert_eq!(safe_origin(uri).as_deref(), Some(origin));
            assert_eq!(
                dispatch_with(uri, |target| {
                    assert_eq!(target, uri);
                }),
                Ok(())
            );
        }
    }

    #[test]
    fn only_explicit_recovery_actions_return_the_original_bytes() {
        let uri = "HTTPS://Example.COM:00443/path?token=secret#fragment";
        assert_eq!(safe_origin(uri).as_deref(), Some("https://example.com:443"));
        for response in [None, Some(0), Some(3), Some(-1)] {
            assert_eq!(recovery_action(response, true, uri), RecoveryAction::None);
        }
        assert_eq!(
            recovery_action(Some(1), true, uri),
            RecoveryAction::Retry(uri)
        );
        assert_eq!(
            recovery_action(Some(2), true, uri),
            RecoveryAction::Copy(uri)
        );
        for response in [Some(1), Some(2)] {
            assert_eq!(recovery_action(response, false, uri), RecoveryAction::None);
            assert_eq!(
                recovery_action(response, true, "file:///etc/passwd"),
                RecoveryAction::None
            );
        }
    }

    #[test]
    fn pending_launch_completions_in_either_order_cannot_replace_current_prompt() {
        for newer_first in [false, true] {
            let mut owner = RecoveryOwner::default();
            let old = owner.begin();
            let current = owner.begin();
            let failure = LaunchOutcome::Failed(OpenFailure::PermissionDenied);
            if !newer_first {
                assert_eq!(owner.complete_launch(&old, true, failure), (false, None));
            }
            assert_eq!(
                owner.complete_launch(&current, true, failure),
                (true, Some(OpenFailure::PermissionDenied))
            );
            let prompt = owner.begin();
            if newer_first {
                assert_eq!(owner.complete_launch(&old, true, failure), (false, None));
            }
            assert_eq!(
                owner.complete_launch(&current, true, failure),
                (false, None)
            );
            assert!(
                owner.consume(&prompt),
                "late failure cannot clear newer prompt"
            );
        }
    }

    #[test]
    fn cancelled_successful_unmapped_and_revoked_launches_show_nothing() {
        let mut owner = RecoveryOwner::default();
        for outcome in [LaunchOutcome::Cancelled, LaunchOutcome::Opened] {
            let ticket = owner.begin();
            assert_eq!(owner.complete_launch(&ticket, true, outcome), (true, None));
            assert_eq!(
                owner.complete_launch(&ticket, true, LaunchOutcome::Failed(OpenFailure::Failed)),
                (false, None)
            );
        }
        let ticket = owner.begin();
        assert_eq!(
            owner.complete_launch(&ticket, false, LaunchOutcome::Failed(OpenFailure::Failed)),
            (true, None)
        );
        let ticket = owner.begin();
        owner.clear(); // replacement/unmap/content mutation invalidates before cancel.
        let newer = owner.begin();
        assert_eq!(
            owner.complete_launch(&ticket, true, LaunchOutcome::Failed(OpenFailure::Failed)),
            (false, None)
        );
        assert!(owner.consume(&newer));
    }

    #[test]
    fn delayed_dispatch_retains_original_bytes_without_implicit_retry() {
        let uri = "HTTPS://Example.COM:00443/private?token=query-secret#fragment-secret";
        let mut owner = RecoveryOwner::default();
        let ticket = owner.begin();
        let mut dispatched = Vec::new();
        assert_eq!(
            dispatch_with(uri, |target| dispatched.push(target.to_owned())),
            Ok(())
        );
        assert_eq!(dispatched, [uri]);
        let failure = owner.complete_launch(
            &ticket,
            true,
            LaunchOutcome::Failed(OpenFailure::MissingHandler),
        );
        assert_eq!(failure, (true, Some(OpenFailure::MissingHandler)));
        assert_eq!(dispatched.len(), 1);
        let RecoveryAction::Retry(target) = recovery_action(Some(1), true, &dispatched[0]) else {
            panic!("explicit retry");
        };
        let retried = target.to_owned();
        dispatch_with(&retried, |target| dispatched.push(target.to_owned())).unwrap();
        assert_eq!(dispatched, [uri, uri]);
        assert_eq!(
            recovery_action(Some(2), true, &dispatched[0]),
            RecoveryAction::Copy(uri)
        );
    }

    #[test]
    fn replacement_dismissal_and_owner_loss_revoke_old_actions() {
        let mut owner = RecoveryOwner::default();
        let old = owner.begin();
        let current = owner.begin();
        assert!(!owner.consume(&old));
        assert!(owner.consume(&current));
        assert!(!owner.consume(&current));
        let unmapped = owner.begin();
        owner.clear();
        assert!(!owner.consume(&unmapped));
        let remapped = owner.begin();
        assert!(!owner.consume(&unmapped));
        assert!(owner.consume(&remapped));
    }
}
