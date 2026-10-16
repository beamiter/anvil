//! Uses the application's actual save/load serialization in fresh test processes.
//! The ordinary job is paused just before sync_all with a channel, not by a
//! kernel/filesystem fault. This is crash/restart evidence, not power-loss or
//! native GTK force_quit acceptance. Every file belongs to this test.
use super::*;
use std::fs::{self, File};
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

const CHILD_TEST: &str = "persistence::restart_tests::checkpoint_child_process";
const ROLE_ENV: &str = "ANVIL_A13_CHECKPOINT_TEST_ROLE";
const ROOT_ENV: &str = "ANVIL_A13_CHECKPOINT_TEST_ROOT";

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn spawn_child(root: &Path, role: &str) -> OwnedChild {
    let stdout = File::create(root.join(format!("{role}.stdout"))).unwrap();
    let stderr = File::create(root.join(format!("{role}.stderr"))).unwrap();
    OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
            .env(ROLE_ENV, role)
            .env(ROOT_ENV, root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap(),
    )
}

fn wait_success(child: &mut OwnedChild) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "checkpoint child exited: {status}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "checkpoint child exceeded watchdog"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn snapshot(title: &str) -> crate::session::SavedSession {
    crate::session::SavedSession {
        active: 0,
        tabs: vec![crate::session::SavedTab {
            title: title.into(),
            custom_title: true,
            pinned: false,
            private_title: false,
            layout: crate::session::PaneLayout::empty_leaf(),
        }],
        ai_conversation: None,
    }
}

#[test]
fn checkpoint_child_process() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("owned child test root"));
    assert!(root.is_dir());
    match role.as_str() {
        "seed" => {
            crate::session::save_session(snapshot("old workspace"));
            shutdown(Duration::from_secs(3)).unwrap();
            assert!(drain_failures().is_empty());
        }
        "writer" => {
            let previous = crate::session::load_session().expect("seed workspace restored");
            assert_eq!(previous.tabs[0].title, "old workspace");
            let (ordinary_started_tx, ordinary_started_rx) = mpsc::channel();
            let (_ordinary_release_tx, ordinary_release_rx) = mpsc::channel::<()>();
            let history = root.join("owned-history");
            enqueue(
                PersistenceKey::for_path("history", &history),
                "blocked history sync",
                move || {
                    let mut file = File::create(history)?;
                    file.write_all(b"owned fault-injection history")?;
                    ordinary_started_tx.send(()).unwrap();
                    // Deterministic pre-fsync stall; sender remains alive until kill.
                    ordinary_release_rx.recv().unwrap();
                    file.sync_all()
                },
            )
            .unwrap();
            ordinary_started_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            let (session_started_tx, session_started_rx) = mpsc::channel();
            let (session_release_tx, session_release_rx) = mpsc::channel();
            enqueue_session(
                PersistenceKey::named("a13-session-gate"),
                "session gate",
                move || {
                    session_started_tx.send(()).unwrap();
                    session_release_rx.recv().unwrap();
                    Ok(())
                },
            )
            .unwrap();
            session_started_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            crate::session::save_session(snapshot("superseded workspace"));
            crate::session::save_session(snapshot("final workspace"));
            session_release_tx.send(()).unwrap();
            for timeout in [
                Duration::from_secs(3),
                Duration::from_millis(20),
                Duration::from_millis(20),
            ] {
                let error = shutdown(timeout).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                assert!(error.to_string().starts_with("ordinary persistence worker"));
            }
            assert!(
                drain_failures().is_empty(),
                "queueing or serialization failed"
            );
            // A successful session join has already included atomic replace and
            // file/directory sync. Parent kills us with ordinary work still stuck.
            fs::write(
                root.join("checkpoint-ready"),
                b"session joined; ordinary timed out",
            )
            .unwrap();
            loop {
                thread::park();
            }
        }
        "reader" => {
            let restored = crate::session::load_session().expect("recover final checkpoint");
            assert_eq!(restored.tabs.len(), 1);
            assert_eq!(restored.tabs[0].title, "final workspace");
            // Keep the same contract as startup: adopt the claim, then checkpoint.
            crate::session::save_session(restored);
            shutdown(Duration::from_secs(3)).unwrap();
            assert!(drain_failures().is_empty());
        }
        other => panic!("unknown checkpoint test role: {other}"),
    }
}

#[test]
fn fresh_process_restores_final_checkpoint_after_blocked_ordinary_writer_is_killed() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "anvil-a13-checkpoint-{}-{nonce}",
        std::process::id()
    ));
    // create_dir, not create_dir_all: never reuse an existing data directory.
    fs::create_dir(&root).unwrap();
    let mut seed = spawn_child(&root, "seed");
    wait_success(&mut seed);
    let mut writer = spawn_child(&root, "writer");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !root.join("checkpoint-ready").exists() {
        assert!(
            writer.0.try_wait().unwrap().is_none(),
            "writer failed; logs in {}",
            root.display()
        );
        assert!(
            Instant::now() < deadline,
            "writer exceeded watchdog; logs in {}",
            root.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
    writer.0.kill().unwrap();
    let status = writer.0.wait().unwrap();
    assert!(
        !status.success(),
        "writer must be killed, not cleanly exited"
    );
    let mut reader = spawn_child(&root, "reader");
    wait_success(&mut reader);
    // Only remove the exact unique directory created by this test after success.
    fs::remove_dir_all(&root).unwrap();
}
