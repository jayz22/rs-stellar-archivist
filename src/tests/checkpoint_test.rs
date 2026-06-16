use crate::checkpoint::{Checkpointer, RenderFn};
use crate::report::{Progress, RunStatus};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

use super::utils::{file_url_from_path, testnet_small_archive_path};
use crate::test_helpers::ScanConfig;

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

#[test]
fn failure_tracker_union_preserves_and_merges() {
    use crate::utils::{FailureTracker, FileFlags};
    let mut a = FailureTracker::default();
    a.record_checkpoint(127);
    a.record_file(63, FileFlags::LEDGER);
    let mut b = FailureTracker::default();
    b.record_checkpoint(191);                 // new
    b.record_file(63, FileFlags::RESULTS);    // merges into cp 63
    a.union_from(&b);
    assert!(a.checkpoints.contains(&127) && a.checkpoints.contains(&191)); // preserved + merged
    assert!(a.files.get(&63).unwrap().has(FileFlags::LEDGER));
    assert!(a.files.get(&63).unwrap().has(FileFlags::RESULTS));
}

#[test]
fn checkpointer_load_reads_prior_failures() {
    use crate::report::{ArchiveReport, write_to_path_atomic};
    use crate::checkpoint::Checkpointer;
    use crate::utils::FailureTracker;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.json");
    let mut t = FailureTracker::default();
    t.record_checkpoint(127);
    write_to_path_atomic(&path, &ArchiveReport::from_failures_and_summary(&t, Default::default())).unwrap();
    let loaded = Checkpointer::load(&path).unwrap();
    assert!(loaded.checkpoints.contains(&127));
}

/// Integration test: scan the local testnet-archive-small fixture with
/// checkpoint-interval 1 and a report path, then verify the written report
/// has `run_status == Complete` (finalize overwrites the last periodic flush)
/// and that `progress.processed_checkpoints == progress.total_checkpoints > 0`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_report_has_progress_and_complete_status() {
    let dir = tempfile::tempdir().unwrap();
    let report_path = dir.path().join("scan-report.json");

    let archive_url = file_url_from_path(&testnet_small_archive_path());

    // Bound the scan to the first four checkpoints in the fixture so the test
    // stays fast. 0x6ff = 1791 covers 0x63f, 0x67f, 0x6bf, 0x6ff.
    let config = ScanConfig::new(&archive_url)
        .skip_optional()
        .high(0x6ff);

    let src_store = crate::storage::from_url_with_config(
        &config.archive,
        &config.storage_config,
    )
    .unwrap();
    let pipeline_config = crate::pipeline::PipelineConfig {
        concurrency: config.concurrency,
        skip_optional: config.skip_optional,
        skip_history_and_buckets: false,
        verify: config.verify,
        storage_config: config.storage_config.clone(),
    };
    let operation = crate::scan_operation::ScanOperation::new(
        config.low,
        config.high,
        pipeline_config.clone(),
    );
    let mut pipeline = crate::pipeline::Pipeline::new(
        operation,
        pipeline_config,
        src_store,
        None,
        Some(report_path.clone()),
    );

    // Wire the checkpointer at interval=1 so every checkpoint triggers a flush.
    // Backstop is set high so only the count gate fires during the run.
    let cp = std::sync::Arc::new(crate::checkpoint::single_section_checkpointer(
        report_path.clone(),
        /*interval*/ 1,
        std::time::Duration::from_secs(3600),
        pipeline.stats_arc(),
    ));
    pipeline.set_checkpointer(cp);

    pipeline
        .run()
        .await
        .map_err(crate::utils::map_pipeline_error)
        .expect("scan should succeed on the small testnet fixture");

    // Read back the report written by finalize (last writer wins).
    let report = crate::report::read_from_path(&report_path).unwrap();

    // Finalize must overwrite the last periodic flush with Complete status.
    assert_eq!(
        report.run_status,
        crate::report::RunStatus::Complete,
        "finalize must overwrite the last periodic flush with Complete"
    );

    assert!(
        report.progress.total_checkpoints > 0,
        "total_checkpoints must be populated by the pipeline"
    );
    assert_eq!(
        report.progress.processed_checkpoints,
        report.progress.total_checkpoints,
        "processed == total at end of a complete run"
    );
}
