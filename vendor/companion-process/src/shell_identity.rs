//! Kernel executable identity for an interactive PTY root. A matching program
//! is necessary, not sufficient, for idle: the frontend must also observe a
//! fresh empty semantic prompt. In particular a shell builtin or a same-image
//! exec can have the original PID, group, session and executable while busy.
use std::{fs::File, path::Path};

#[derive(Debug)]
pub struct ShellIdentity {
    program: Option<File>,
    pid: i32,
    start_ticks: Option<u64>,
}

impl ShellIdentity {
    /// Keep the expected program open so replacement/deletion of its path does
    /// not authorize a different executable. Bind to this child incarnation.
    /// A jsh bootstrap may still be bash here: it never becomes an idle shell
    /// until the expected program and a fresh prompt have both been observed.
    pub fn capture(pid: i32, program: Option<&Path>) -> Self {
        Self {
            program: program.and_then(|path| File::open(path).ok()),
            pid,
            start_ticks: process_start(pid),
        }
    }

    #[cfg(target_os = "linux")]
    pub fn matches_current_program(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Some(start) = self.start_ticks else {
            return false;
        };
        if process_start(self.pid) != Some(start) {
            return false;
        }
        let Some(expected) = self.program.as_ref().and_then(|file| file.metadata().ok()) else {
            return false;
        };
        let Ok(current) = std::fs::metadata(format!("/proc/{}/exe", self.pid)) else {
            return false;
        };
        expected.dev() == current.dev()
            && expected.ino() == current.ino()
            && process_start(self.pid) == Some(start)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn matches_current_program(&self) -> bool {
        // No unverified path/name fallback: explicit shell/tab exit remains
        // available on a platform without this observation implementation.
        let _ = (&self.program, self.pid, self.start_ticks);
        false
    }
}

#[cfg(target_os = "linux")]
fn process_start(pid: i32) -> Option<u64> {
    use std::io::Read;
    if pid <= 1 {
        return None;
    }
    let mut raw = String::new();
    File::open(format!("/proc/{pid}/stat"))
        .ok()?
        .take(16 * 1024)
        .read_to_string(&mut raw)
        .ok()?;
    let fields: Vec<_> = raw.get(raw.rfind(')')? + 1..)?.split_whitespace().collect();
    if matches!(*fields.first()?, "Z" | "X" | "x") {
        return None;
    }
    fields.get(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn process_start(_pid: i32) -> Option<u64> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };

    struct TestChild(Child);
    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn ready_shell(program: &Path, command: &str) -> TestChild {
        let mut child = TestChild(
            Command::new(program)
                .args(["-c", command])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        child
    }

    #[test]
    fn actual_exec_changes_program_without_changing_child_incarnation() {
        let mut owned = ready_shell(
            Path::new("/bin/sh"),
            "printf 'ready\\n'; read value; exec /bin/sleep 30",
        );
        let child = &mut owned.0;
        let identity = ShellIdentity::capture(child.id() as i32, Some(Path::new("/bin/sh")));
        assert!(identity.matches_current_program());
        let start = identity.start_ticks;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"continue\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while identity.matches_current_program() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("actual shell exec never changed its executable");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(process_start(child.id() as i32), start);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!identity.matches_current_program());
    }

    #[test]
    fn path_replacement_and_spoofed_basename_do_not_authorize_another_program() {
        let root =
            std::env::temp_dir().join(format!("eva-shell-identity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let shell = root.join("sh");
        std::fs::copy("/bin/sh", &shell).unwrap();
        let mut owned = ready_shell(&shell, "printf 'ready\\n'; read value");
        let child = &mut owned.0;
        let identity = ShellIdentity::capture(child.id() as i32, Some(&shell));
        assert!(identity.matches_current_program());
        std::fs::remove_file(&shell).unwrap();
        std::fs::copy("/bin/sleep", &shell).unwrap();
        assert!(
            identity.matches_current_program(),
            "open original inode survives deletion"
        );
        let other = ShellIdentity::capture(child.id() as i32, Some(&shell));
        assert!(
            !other.matches_current_program(),
            "the same path now names different code"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_identity_or_wrong_start_never_proves_a_shell() {
        let pid = std::process::id() as i32;
        let mut identity = ShellIdentity::capture(pid, Some(Path::new("/proc/self/exe")));
        assert!(identity.matches_current_program());
        identity.start_ticks = identity.start_ticks.map(|value| value + 1);
        assert!(!identity.matches_current_program());
        assert!(!ShellIdentity::capture(pid, None).matches_current_program());
        assert!(!ShellIdentity::capture(-1, Some(Path::new("/bin/sh"))).matches_current_program());
    }
}
