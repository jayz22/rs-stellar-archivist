use crate::checkpoint::{Checkpointer, RenderFn};
use crate::report::{Progress, RunStatus};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

fn counting_render(calls: Arc<AtomicU64>) -> RenderFn {
    Arc::new(move |status: RunStatus, progress: Progress| {
        let calls = calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(serde_json::json!({
                "run_status": status,
                "progress": progress,
                "version": 1
            }))
        })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maybe_flush_triggers_on_interval_and_flush_now_writes_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.json");
    let calls = Arc::new(AtomicU64::new(0));
    let cp = Checkpointer::new(
        path.clone(),
        /*interval*/ 5,
        /*backstop*/ std::time::Duration::from_secs(3600),
        /*total*/ 10,
        counting_render(calls.clone()),
    );

    cp.maybe_flush(1).await; // below interval, no write
    cp.maybe_flush(4).await; // still below
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(!path.exists());

    cp.maybe_flush(5).await; // crosses interval -> one write
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    // periodic flushes use Interrupted (run isn't complete until finalize)
    assert_eq!(v["run_status"], "interrupted");
    assert_eq!(v["progress"]["total_checkpoints"], 10);

    cp.flush_now(RunStatus::Interrupted).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(v["run_status"], "interrupted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maybe_flush_triggers_on_time_backstop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.json");
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let cp = Checkpointer::new(
        path.clone(),
        /*interval*/ 0,                                   // count gate OFF
        /*backstop*/ std::time::Duration::from_millis(1),
        /*total*/ 4,
        counting_render(calls.clone()),
    );
    tokio::time::sleep(std::time::Duration::from_millis(10)).await; // exceed backstop
    cp.maybe_flush(2).await; // interval=0 so only the time gate can fire
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(path.exists());
}
