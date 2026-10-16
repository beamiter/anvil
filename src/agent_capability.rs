//! Read-only explanations, never authority to execute or relax a guard.
//! These values contain no token, environment, or terminal text.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentExecutionLimitation {
    HostBridge,
    IntegrationDisabled,
    PlatformUnsupported,
    EntropyUnavailable,
    ForegroundUnknown,
    IntegrationUnverified,
}

/// Capture launch-time capability only. A missing token cannot recover in
/// this PTY: a new trusted session must obtain its own token.
pub(crate) fn launch_limitation(
    host_bridge: bool,
    integration_enabled: bool,
    supported_platform: bool,
    token_created: bool,
) -> Option<AgentExecutionLimitation> {
    use AgentExecutionLimitation::*;
    if host_bridge {
        Some(HostBridge)
    } else if !integration_enabled {
        Some(IntegrationDisabled)
    } else if !supported_platform {
        Some(PlatformUnsupported)
    } else if !token_created {
        Some(EntropyUnavailable)
    } else {
        None
    }
}

pub(crate) fn execution_limitation(
    launch: Option<AgentExecutionLimitation>,
    foreground_known: bool,
    integration_verified: bool,
) -> Option<AgentExecutionLimitation> {
    launch.or({
        if !foreground_known {
            Some(AgentExecutionLimitation::ForegroundUnknown)
        } else if !integration_verified {
            Some(AgentExecutionLimitation::IntegrationUnverified)
        } else {
            None
        }
    })
}

impl AgentExecutionLimitation {
    pub(crate) fn short_label(self) -> &'static str {
        match self {
            Self::HostBridge => "Host bridge: execution unavailable",
            Self::IntegrationDisabled => "Trusted shell integration unavailable",
            Self::PlatformUnsupported => "Trusted token unsupported",
            Self::EntropyUnavailable => "Secure token unavailable",
            Self::ForegroundUnknown => "Foreground identity unknown",
            Self::IntegrationUnverified => "Shell integration unverified",
        }
    }

    pub(crate) fn blocked_message(self) -> &'static str {
        match self {
            Self::HostBridge => "Agent execution is unavailable in this Flatpak host-bridge pane: shell identity cannot be verified across the bridge. Use a new direct local bash/zsh pane outside the host bridge with bundled shell integration. Commands still require your explicit approval.",
            Self::IntegrationDisabled => "This pane was started without trusted shell integration. Open a new direct local bash/zsh pane with the bundled integration for Agent execution. Enabling integration cannot upgrade this existing session.",
            Self::PlatformUnsupported => "Secure shell-token delivery is unsupported on this platform. Agent execution remains unavailable. Use a supported Linux installation with a new direct local bash/zsh pane and bundled integration.",
            Self::EntropyUnavailable => "A secure shell token could not be created because operating-system randomness was unavailable or incomplete at launch. This pane cannot gain Agent execution later. Once system randomness is available, open a new direct local bash/zsh pane with bundled integration; no command will run automatically.",
            Self::ForegroundUnknown => "The foreground shell identity cannot currently be verified. Wait for a normal shell prompt; if this persists, open a new direct local bash/zsh pane with bundled integration. Execution stays blocked until identity and the empty prompt are verified.",
            Self::IntegrationUnverified => "The bundled token-aware shell integration has not been verified. Wait for it to initialize; if this persists, enable the bundled bash/zsh integration and open a new direct local pane. A token alone never permits execution; your explicit approval is still required.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_reasons_are_distinct_and_fail_closed() {
        use AgentExecutionLimitation::*;
        assert_eq!(launch_limitation(true, true, true, false), Some(HostBridge));
        assert_eq!(
            launch_limitation(false, false, true, false),
            Some(IntegrationDisabled)
        );
        assert_eq!(
            launch_limitation(false, true, false, false),
            Some(PlatformUnsupported)
        );
        assert_eq!(
            launch_limitation(false, true, true, false),
            Some(EntropyUnavailable)
        );
        assert_eq!(launch_limitation(false, true, true, true), None);
    }

    #[test]
    fn existing_session_cannot_recover_missing_launch_capability() {
        let launch = launch_limitation(false, true, true, false);
        for foreground in [false, true] {
            for verified in [false, true] {
                assert_eq!(execution_limitation(launch, foreground, verified), launch);
            }
        }
        let fresh = launch_limitation(false, true, true, true);
        assert_eq!(execution_limitation(fresh, true, true), None);
    }

    #[test]
    fn normal_local_session_still_needs_foreground_and_integration_proof() {
        use AgentExecutionLimitation::*;
        assert_eq!(
            execution_limitation(None, false, true),
            Some(ForegroundUnknown)
        );
        assert_eq!(
            execution_limitation(None, false, false),
            Some(ForegroundUnknown)
        );
        assert_eq!(
            execution_limitation(None, true, false),
            Some(IntegrationUnverified)
        );
        assert_eq!(execution_limitation(None, true, true), None);
    }

    #[test]
    fn explanations_are_static_distinct_and_actionable() {
        use AgentExecutionLimitation::*;
        let reasons = [
            HostBridge,
            IntegrationDisabled,
            PlatformUnsupported,
            EntropyUnavailable,
            ForegroundUnknown,
            IntegrationUnverified,
        ];
        for (i, reason) in reasons.iter().enumerate() {
            assert!(reason.blocked_message().contains("pane"));
            for other in &reasons[..i] {
                assert_ne!(reason.short_label(), other.short_label());
                assert_ne!(reason.blocked_message(), other.blocked_message());
            }
        }
    }
}
