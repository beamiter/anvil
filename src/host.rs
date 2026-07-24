//! Host integration for native and Flatpak launches.
//!
//! Terminal children must run on the host when jterm1 itself is sandboxed;
//! otherwise users would receive a shell inside the application runtime.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const APP_ID: &str = "io.github.beamiter.jterm1";

pub(crate) fn is_flatpak() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| {
        std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").is_file()
    })
}

fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

pub(crate) fn bridge_available() -> bool {
    !is_flatpak()
        || Path::new("/usr/bin/flatpak-spawn").is_file()
        || find_executable_in_path("flatpak-spawn").is_some()
}

#[derive(Default)]
struct CwdProbeCache {
    entries: HashMap<String, (Instant, bool)>,
    bridge_timeout_until: Option<Instant>,
}

#[derive(Clone, Copy)]
enum ProbeOutcome {
    Finished(bool),
    TimedOut,
}

/// Check a requested terminal cwd in the same filesystem namespace where the
/// child will run. A Flatpak may not be able to stat an otherwise valid host
/// directory directly, so ask the host bridge instead.
pub(crate) fn working_directory_available(path: &str) -> bool {
    if !is_flatpak() {
        return Path::new(path).is_dir();
    }

    const CACHE_TTL: Duration = Duration::from_secs(2);
    const MAX_CACHE_ENTRIES: usize = 256;
    static CACHE: OnceLock<Mutex<CwdProbeCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(CwdProbeCache::default()));
    if let Ok(cache) = cache.lock() {
        if cache
            .bridge_timeout_until
            .is_some_and(|until| until > Instant::now())
        {
            return false;
        }
        if let Some((checked_at, available)) = cache.entries.get(path) {
            if checked_at.elapsed() < CACHE_TTL {
                return *available;
            }
        }
    }

    let mut check = command("test");
    check
        .args(["-d", path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let outcome = status_with_timeout(check, Duration::from_millis(250));
    let available = matches!(outcome, ProbeOutcome::Finished(true));

    if let Ok(mut cache) = cache.lock() {
        cache
            .entries
            .retain(|_, (checked_at, _)| checked_at.elapsed() < CACHE_TTL);
        if cache.entries.len() >= MAX_CACHE_ENTRIES {
            cache.entries.clear();
        }
        if matches!(outcome, ProbeOutcome::TimedOut) {
            // One wedged bridge implies every immediately following path probe
            // would hit the same timeout. Bound an N-pane restore to one wait.
            cache.bridge_timeout_until = Some(Instant::now() + CACHE_TTL);
        } else {
            cache.bridge_timeout_until = None;
        }
        cache
            .entries
            .insert(path.to_string(), (Instant::now(), available));
    }
    available
}

/// Wait for a small host probe without ever letting a missing or wedged bridge
/// freeze GTK's main thread indefinitely.
fn status_with_timeout(mut command: Command, timeout: Duration) -> ProbeOutcome {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            log::warn!("failed to start host working-directory probe: {error}");
            return ProbeOutcome::Finished(false);
        }
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return ProbeOutcome::Finished(status.success()),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                log::warn!("host working-directory probe timed out");
                terminate_probe(child);
                return ProbeOutcome::TimedOut;
            }
            Err(error) => {
                log::warn!("host working-directory probe failed: {error}");
                terminate_probe(child);
                return ProbeOutcome::Finished(false);
            }
        }
    }
}

fn terminate_probe(mut child: std::process::Child) {
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_millis(50);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    if let Err(error) = std::thread::Builder::new()
        .name("jterm1-host-probe-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        })
    {
        log::warn!("failed to start host-probe reaper: {error}");
    }
}

fn wrap_argv_for(
    flatpak: bool,
    argv: &[String],
    cwd: Option<&str>,
    env_extra: &[(&str, &str)],
) -> Vec<String> {
    if !flatpak {
        return argv.to_vec();
    }

    let mut wrapped = vec![
        "flatpak-spawn".to_string(),
        "--host".to_string(),
        "--watch-bus".to_string(),
    ];
    if let Some(cwd) = cwd.filter(|value| !value.is_empty()) {
        wrapped.push(format!("--directory={cwd}"));
    }
    wrapped.push("--env=TERM=xterm-256color".to_string());
    for (key, value) in env_extra {
        wrapped.push(format!("--env={key}={value}"));
    }
    wrapped.extend(argv.iter().cloned());
    wrapped
}

pub(crate) fn wrap_argv(
    argv: &[String],
    cwd: Option<&str>,
    env_extra: &[(&str, &str)],
) -> Vec<String> {
    wrap_argv_for(is_flatpak(), argv, cwd, env_extra)
}

pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    if is_flatpak() {
        let mut command = Command::new("flatpak-spawn");
        command.args(["--host", "--watch-bus"]);
        command.arg(program);
        command
    } else {
        Command::new(program)
    }
}

pub(crate) fn command_with_cwd(program: impl AsRef<OsStr>, cwd: &Path) -> Command {
    if is_flatpak() {
        let mut command = Command::new("flatpak-spawn");
        command.args(["--host", "--watch-bus"]);
        command.arg(format!("--directory={}", cwd.display()));
        command.arg(program);
        command
    } else {
        let mut command = Command::new(program);
        command.current_dir(cwd);
        command
    }
}

pub(crate) fn command_available(name: &str) -> bool {
    if !is_flatpak() {
        return find_executable_in_path(name).is_some();
    }
    if !bridge_available() {
        return false;
    }

    command("sh")
        .args([
            "-lc",
            "command -v -- \"$1\" >/dev/null 2>&1",
            "jterm1-host-probe",
            name,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_argv_is_unchanged() {
        let argv = vec!["bash".to_string(), "-l".to_string()];
        assert_eq!(
            wrap_argv_for(false, &argv, Some("/tmp"), &[("LESS", "R")]),
            argv
        );
    }

    #[test]
    fn flatpak_argv_routes_cwd_and_environment_to_host() {
        let argv = vec!["bash".to_string(), "-l".to_string()];
        assert_eq!(
            wrap_argv_for(true, &argv, Some("/home/alice/project"), &[("LESS", "R")]),
            vec![
                "flatpak-spawn",
                "--host",
                "--watch-bus",
                "--directory=/home/alice/project",
                "--env=TERM=xterm-256color",
                "--env=LESS=R",
                "bash",
                "-l",
            ]
        );
    }

    #[test]
    fn flatpak_shell_identity_reaches_the_host_child() {
        let argv = vec!["bash".to_string(), "-l".to_string()];
        let wrapped = wrap_argv_for(true, &argv, None, &[("TERM_PROGRAM", "jterm1")]);
        assert!(wrapped
            .iter()
            .any(|argument| argument == "--env=TERM_PROGRAM=jterm1"));
        assert_eq!(&wrapped[wrapped.len() - 2..], ["bash", "-l"]);
    }
}
