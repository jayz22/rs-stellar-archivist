//! Periodic + on-signal checkpointing of an operation's report.

use crate::report::{write_to_path_atomic, Progress, ReportError, RunStatus};
use futures_util::future::BoxFuture;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Operation-supplied: render the current report as JSON for the given status +
/// progress. `scan`/`mirror` build an `ArchiveReport`; `repair` builds its
/// current pass's section. Returns a `serde_json::Value` so one Checkpointer
/// serves all shapes.
pub type RenderFn = std::sync::Arc<
    dyn Fn(RunStatus, Progress) -> BoxFuture<'static, Result<serde_json::Value, ReportError>>
        + Send
        + Sync,
>;

pub struct Checkpointer {
    report_path: PathBuf,
    interval: usize,    // flush every N completed checkpoints (0 = off)
    backstop: Duration, // also flush if >= this since last write
    total: AtomicU64,   // total checkpoints (for Progress); set via set_total
    render: RenderFn,
    inner: Mutex<Inner>, // serializes writes + holds last-flush instant + last_completed
}

struct Inner {
    last_flush_at: Instant,
    last_completed: u64,
}

impl Checkpointer {
    pub fn new(
        report_path: PathBuf,
        interval: usize,
        backstop: Duration,
        total: u64,
        render: RenderFn,
    ) -> Self {
        Self {
            report_path,
            interval,
            backstop,
            total: AtomicU64::new(total),
            render,
            inner: Mutex::new(Inner {
                last_flush_at: Instant::now(),
                last_completed: 0,
            }),
        }
    }

    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    /// Load prior findings from an existing single-section report (for --resume).
    /// Hard error on a missing/unparseable/unsupported report — never guesses.
    pub fn load(path: &std::path::Path) -> Result<crate::utils::FailureTracker, ReportError> {
        let report = crate::report::read_from_path(path)?;
        report.into_failures()
    }

    /// Periodic driver: flush if the interval was crossed or the time backstop
    /// elapsed. Cheap and safe to call from every checkpoint completion.
    /// The flush decision and `last_completed` update happen under a single
    /// lock acquisition to prevent TOCTOU races in concurrent callers.
    pub async fn maybe_flush(&self, completed: usize) {
        let mut inner = self.inner.lock().await;
        let due_by_count = self.interval > 0
            && completed as u64 >= inner.last_completed + self.interval as u64;
        let due_by_time = inner.last_flush_at.elapsed() >= self.backstop;
        if !due_by_count && !due_by_time {
            return;
        }
        let _ = self
            .flush_locked(&mut inner, RunStatus::Interrupted, completed as u64)
            .await;
    }

    /// Force a flush (signal handler). Uses the last-seen completed count.
    pub async fn flush_now(&self, status: RunStatus) -> Result<(), ReportError> {
        let mut inner = self.inner.lock().await;
        let completed = inner.last_completed;
        self.flush_locked(&mut inner, status, completed).await
    }

    /// Write the report while the caller already holds the `inner` lock.
    /// Updates `inner.last_flush_at` and `inner.last_completed` on success.
    /// Must NOT re-acquire `self.inner` — callers hold it.
    async fn flush_locked(
        &self,
        inner: &mut Inner,
        status: RunStatus,
        completed: u64,
    ) -> Result<(), ReportError> {
        let progress = Progress {
            processed_checkpoints: completed,
            total_checkpoints: self.total.load(Ordering::Relaxed),
        };
        let value = (self.render)(status, progress).await?;
        if let Err(e) = write_to_path_atomic(&self.report_path, &value) {
            tracing::warn!(
                "checkpoint write failed for {}: {e}",
                self.report_path.display()
            );
            return Err(e);
        }
        inner.last_flush_at = Instant::now();
        inner.last_completed = completed;
        Ok(())
    }
}
