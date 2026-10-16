//! Fault matrix against the production history reader, saver and atomic helper.
//! All files and child processes belong to an isolated fixture.
use super::*;
use std::process::{Child, Command, Stdio};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "anvil-history-fault-{}-{}",
            std::process::id(),
            next_temp_id()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn history(&self) -> PathBuf {
        self.0.join("history.bin")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child(fixture: &Fixture, mode: &str) -> OwnedChild {
    OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "block_view::history::history_fault_tests::history_fault_child",
                "--ignored",
                "--nocapture",
            ])
            .env("ANVIL_HISTORY_FAULT_FIXTURE", &fixture.0)
            .env("ANVIL_HISTORY_FAULT_MODE", mode)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn wait_ready(fixture: &Fixture, child: &mut OwnedChild) {
    let start = Instant::now();
    while !fixture.0.join("ready").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before checkpoint"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "child checkpoint timeout"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_success(child: &mut OwnedChild) {
    let start = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "recovery process failed: {status}");
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "recovery process hung"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn seed(fixture: &Fixture) -> Vec<u8> {
    // A valid tombstone is a complete persisted history, recognized by the real
    // reader without inventing a parallel record schema.
    let payload = encode_clear_tombstone(11);
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend(payload);
    atomic_write(&fixture.history(), |file| file.write_all(&frame)).unwrap();
    frame
}

fn peak_rss_kib() -> Option<u64> {
    // Linux native CI; /proc is observational and contains no user content.
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|line| line.starts_with("VmHWM:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[test]
#[ignore = "child entry point; invoked only by isolated history fault tests"]
fn history_fault_child() {
    let Some(directory) = std::env::var_os("ANVIL_HISTORY_FAULT_FIXTURE") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let path = directory.join("history.bin");
    let mode = std::env::var("ANVIL_HISTORY_FAULT_MODE").unwrap();
    if mode.starts_with("recover-") {
        let original = fs::read(&path).unwrap();
        let compressed = mode.ends_with("compressed");
        let started = Instant::now();
        let before = peak_rss_kib();
        let loaded = read_history_records(&path, compressed, 16, UndecodablePolicy::Reject);
        if mode.starts_with("recover-corrupt") {
            assert!(loaded.is_err(), "corrupt fixture unexpectedly decoded");
            assert!(save_history_snapshot(&path, &[], compressed, None).is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
        } else {
            let loaded = loaded.unwrap();
            if mode == "recover-records" {
                assert!(!loaded.blocks.is_empty());
                assert!(loaded
                    .blocks
                    .iter()
                    .all(|block| block.cmd == "preserved command"
                        && block.output == "preserved output\n"));
            } else {
                assert_eq!(loaded.clear_tombstone, Some(11));
                assert!(loaded.blocks.is_empty());
            }
        }
        let after = peak_rss_kib();
        // Isolated fresh child: avoid charging another parallel test's heap.
        // This is a regression ceiling, not an exact allocator accounting claim.
        if let (Some(before), Some(after)) = (before, after) {
            assert!(
                after.saturating_sub(before) < 128 * 1024,
                "recovery RSS grew over 128 MiB"
            );
        }
        eprintln!(
            "history {mode}: {:?}, peak RSS KiB {before:?} -> {after:?}",
            started.elapsed()
        );
        return;
    }
    let _lock = HistoryFileLock::acquire(&path).unwrap();
    if mode == "lock" {
        fs::write(directory.join("ready"), b"locked").unwrap();
        loop {
            std::thread::park();
        }
    }
    let bytes = fs::read(&path).unwrap();
    atomic_write_checkpoints(
        &path,
        |file| {
            // Duplicate valid tombstones distinguish a complete replacement from
            // the old file while preserving the same logical history.
            file.write_all(&bytes)?;
            file.write_all(&bytes)
        },
        |stage| {
            if stage == mode {
                fs::write(directory.join("ready"), stage).unwrap();
                loop {
                    std::thread::park();
                }
            }
            Ok(())
        },
    )
    .unwrap();
}

#[test]
fn history_process_death_at_atomic_boundaries_recovers_complete_file() {
    crash_matrix(false);
}

#[test]
fn history_nonempty_records_process_death_recovers_complete_file() {
    crash_matrix(true);
}

fn crash_matrix(nonempty: bool) {
    for stage in [
        "created", "written", "flushed", "synced", "renamed", "durable",
    ] {
        let fixture = Fixture::new();
        let old = if nonempty {
            let block: BlockData = serde_json::from_value(serde_json::json!({
                "id": 1, "prompt": "$ ", "cmd": "preserved command", "cmd_markup": null,
                "output": "preserved output\n", "exit_code": 0,
                "estimated_height": 2, "line_count": 1
            }))
            .unwrap();
            save_history_snapshot(&fixture.history(), &[block], false, None).unwrap();
            fs::read(fixture.history()).unwrap()
        } else {
            seed(&fixture)
        };
        let mut writer = child(&fixture, stage);
        wait_ready(&fixture, &mut writer);
        writer.0.kill().unwrap(); // Only this test's owned child PID.
        assert!(!writer.0.wait().unwrap().success());
        let actual = fs::read(fixture.history()).unwrap();
        let expected = if matches!(stage, "renamed" | "durable") {
            old.repeat(2)
        } else {
            old
        };
        assert_eq!(actual, expected, "torn publication at {stage}");
        // A fresh process, not inherited state, verifies decode/recovery exit.
        wait_success(&mut child(
            &fixture,
            if nonempty {
                "recover-records"
            } else {
                "recover-valid"
            },
        ));
        let _lock = HistoryFileLock::acquire(&fixture.history()).unwrap();
        eprintln!("history crash checkpoint {stage}: intact bytes, fresh recovery, released locks");
    }
}

#[test]
fn history_enospc_boundaries_preserve_old_file_and_remove_temp() {
    for stage in ["created", "partial-write", "written", "flushed", "synced"] {
        let fixture = Fixture::new();
        let old = seed(&fixture);
        let error = atomic_write_checkpoints(
            &fixture.history(),
            |file| {
                file.write_all(b"partial")?;
                if stage == "partial-write" {
                    return Err(io::Error::from_raw_os_error(nix::libc::ENOSPC));
                }
                Ok(())
            },
            |at| {
                if at == stage {
                    Err(io::Error::from_raw_os_error(nix::libc::ENOSPC))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(nix::libc::ENOSPC));
        assert_eq!(fs::read(fixture.history()).unwrap(), old);
        assert_eq!(
            fs::read_dir(&fixture.0).unwrap().count(),
            1,
            "orphan temp at {stage}"
        );
    }
}

#[test]
fn history_real_process_lock_timeout_is_bounded_and_retry_recovers() {
    let fixture = Fixture::new();
    let old = seed(&fixture);
    let mut holder = child(&fixture, "lock");
    wait_ready(&fixture, &mut holder);
    let started = Instant::now();
    let error = save_history_snapshot(&fixture.history(), &[], false, None)
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() >= HISTORY_LOCK_TIMEOUT);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(fs::read(fixture.history()).unwrap(), old);
    holder.0.kill().unwrap();
    holder.0.wait().unwrap();
    wait_success(&mut child(&fixture, "recover-valid"));
    let loaded =
        read_history_records(&fixture.history(), false, 16, UndecodablePolicy::Reject).unwrap();
    save_history_snapshot_with_intent(
        &fixture.history(),
        &[],
        false,
        SaveIntent::Revision {
            expected_revision: Some(loaded.revision),
            observed_clear_tombstone: loaded.clear_tombstone,
        },
    )
    .unwrap();
    assert_eq!(fs::read(fixture.history()).unwrap(), old);
}

#[test]
fn history_corruption_and_compressed_oversize_survive_recovery_exit() {
    let fixture = Fixture::new();
    for bytes in [
        vec![0xff],
        (MAX_ENCODED_RECORD_BYTES as u32 + 1).to_le_bytes().to_vec(),
        vec![8, 0, 0, 0, 1, 2],
    ] {
        fs::write(fixture.history(), &bytes).unwrap();
        wait_success(&mut child(&fixture, "recover-corrupt"));
        assert_eq!(fs::read(fixture.history()).unwrap(), bytes);
    }
    // One 16 MiB+1 allocation, compressed to a tiny on-disk frame. No 256 MiB
    // fixture duplication; the actual production per-frame cap is exercised.
    let compressed = zstd::encode_all(&vec![0u8; MAX_DECODED_RECORD_BYTES + 1][..], 1).unwrap();
    let error = decode_record(&compressed, true, MAX_DECODED_RECORD_BYTES)
        .err()
        .expect("oversized expansion must be rejected");
    assert!(
        error.to_string().contains("expands beyond"),
        "wrong rejection: {error}"
    );
    let mut bytes = (compressed.len() as u32).to_le_bytes().to_vec();
    bytes.extend(compressed);
    fs::write(fixture.history(), &bytes).unwrap();
    assert!(read_history_records(&fixture.history(), true, 16, UndecodablePolicy::Reject).is_err());
    wait_success(&mut child(&fixture, "recover-corrupt-compressed"));
    assert_eq!(fs::read(fixture.history()).unwrap(), bytes);
}

#[test]
fn history_post_rename_error_reports_failure_with_complete_recoverable_file() {
    for stage in ["renamed", "durable"] {
        let fixture = Fixture::new();
        let old = seed(&fixture);
        let replacement = old.repeat(2);
        let error = atomic_write_checkpoints(
            &fixture.history(),
            |file| file.write_all(&replacement),
            |at| {
                if at == stage {
                    Err(io::Error::from_raw_os_error(nix::libc::EIO))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(nix::libc::EIO));
        // After rename the replacement is visible even if directory sync fails.
        // Never roll back by deleting it; report the uncertainty to the caller.
        assert_eq!(fs::read(fixture.history()).unwrap(), replacement);
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
        wait_success(&mut child(&fixture, "recover-valid"));
    }
}
