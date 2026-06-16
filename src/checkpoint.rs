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
    interval: usize,   // flush every N completed checkpoints (0 = off)
    backstop: Duration, // also flush if >= this since last write
    total: AtomicU64,  // total checkpoints (for Progress); set via set_total
    render: RenderFn,
    last_completed: AtomicU64,
    inner: Mutex<Inner>, // serializes writes + holds last-flush instant
}

struct Inner {
    last_flush_at: Instant,
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
            last_completed: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                last_flush_at: Instant::now(),
            }),
        }
    }

    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    /// Periodic driver: flush if the interval was crossed or the time backstop
    /// elapsed. Cheap and safe to call from every checkpoint completion.
    pub async fn maybe_flush(&self, completed: usize) {
        let due_by_count = self.interval > 0
            && completed as u64
                >= self.last_completed.load(Ordering::Relaxed) + self.interval as u64;
        if !due_by_count {
            let inner = self.inner.lock().await;
            if inner.last_flush_at.elapsed() < self.backstop {
                return;
            }
            drop(inner);
        }
        let _ = self.flush(RunStatus::Interrupted, completed as u64).await;
        self.last_completed.store(completed as u64, Ordering::Relaxed);
    }

    /// Force a flush (signal handler). Uses the last-seen completed count.
    pub async fn flush_now(&self, status: RunStatus) -> Result<(), ReportError> {
        let completed = self.last_completed.load(Ordering::Relaxed);
        self.flush(status, completed).await
    }

    async fn flush(&self, status: RunStatus, completed: u64) -> Result<(), ReportError> {
        let mut inner = self.inner.lock().await; // serialize writes
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
        Ok(())
    }
}
