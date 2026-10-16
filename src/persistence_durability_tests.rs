//! Test-only lane contracts. No GUI or external service is involved.
use super::*;
use std::sync::mpsc;

#[test]
fn repeated_shutdown_reports_blocked_ordinary_lane_without_losing_session_completion() {
    let workers = PersistenceWorkers::new(2, 1).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    workers
        .enqueue(
            PersistenceKey::named("blocked-history"),
            "blocked history".into(),
            Box::new(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            }),
        )
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let (saved_tx, saved_rx) = mpsc::channel();
    workers
        .enqueue_session(
            PersistenceKey::named("workspace"),
            "final workspace".into(),
            Box::new(move || {
                saved_tx.send(()).unwrap();
                Ok(())
            }),
        )
        .unwrap();
    for _ in 0..3 {
        let start = Instant::now();
        let error = workers.shutdown(Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().starts_with("ordinary persistence worker"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    saved_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(workers.session.shared.state.lock().unwrap().exited);
    release_tx.send(()).unwrap();
    workers.shutdown(Duration::from_secs(2)).unwrap();
    workers.shutdown(Duration::ZERO).unwrap();
}

#[test]
fn latest_pending_revision_is_preserved_for_each_distinct_target() {
    let worker = PersistenceWorker::new(3).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    worker
        .enqueue(
            PersistenceKey::named("gate"),
            "gate".into(),
            Box::new(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            }),
        )
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let writes = Arc::new(Mutex::new(Vec::new()));
    for revision in 1..=20 {
        for target in ["a", "b", "c"] {
            let writes = Arc::clone(&writes);
            worker
                .enqueue(
                    PersistenceKey::named(target),
                    format!("{target}:{revision}"),
                    Box::new(move || {
                        writes.lock().unwrap().push((target, revision));
                        Ok(())
                    }),
                )
                .unwrap();
        }
    }
    release_tx.send(()).unwrap();
    worker.shutdown(Duration::from_secs(2)).unwrap();
    assert_eq!(*writes.lock().unwrap(), [("a", 20), ("b", 20), ("c", 20)]);
}

#[test]
fn successful_shutdown_is_not_a_successful_checkpoint_when_task_failed() {
    let workers = PersistenceWorkers::new(1, 1).unwrap();
    workers
        .enqueue_session(
            PersistenceKey::named("workspace"),
            "final workspace".into(),
            Box::new(|| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected checkpoint failure",
                ))
            }),
        )
        .unwrap();
    workers.shutdown(Duration::from_secs(2)).unwrap();
    let failures = workers.drain_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].operation, "final workspace");
    assert_eq!(failures[0].error, "injected checkpoint failure");
}
