fn launch_failure(error: &gtk::glib::Error) -> super::url_recovery::OpenFailure {
    use super::url_recovery::OpenFailure;
    if error.matches(gio::DBusError::AccessDenied) {
        return OpenFailure::PermissionDenied;
    }
    match error.kind::<gio::IOErrorEnum>() {
        Some(gio::IOErrorEnum::NotFound | gio::IOErrorEnum::NotSupported) => {
            OpenFailure::MissingHandler
        }
        Some(gio::IOErrorEnum::PermissionDenied) => OpenFailure::PermissionDenied,
        Some(gio::IOErrorEnum::InvalidArgument) => OpenFailure::InvalidUri,
        _ => OpenFailure::Failed,
    }
}

/// GIO's async completion includes portal/activation failures that a successful
/// synchronous dispatch may miss. Cancellation is a silent terminal outcome.
fn launch_outcome(result: Result<(), gtk::glib::Error>) -> super::url_recovery::LaunchOutcome {
    use super::url_recovery::LaunchOutcome;
    match result {
        Ok(()) => LaunchOutcome::Opened,
        Err(error) if error.matches(gio::IOErrorEnum::Cancelled) => LaunchOutcome::Cancelled,
        Err(error) => LaunchOutcome::Failed(launch_failure(&error)),
    }
}

/// The single production dispatch boundary, also exercised by the isolated
/// native handler/portal fixture. Never call this with a display-only summary.
fn dispatch_uri(
    uri: &str,
    cancellable: &gio::Cancellable,
    completion: impl FnOnce(super::url_recovery::LaunchOutcome) + 'static,
) -> Result<(), super::url_recovery::OpenFailure> {
    super::url_recovery::dispatch_with(uri, |target| {
        gio::AppInfo::launch_default_for_uri_async(
            target,
            None::<&gio::AppLaunchContext>,
            Some(cancellable),
            move |result| completion(launch_outcome(result)),
        );
    })
}

