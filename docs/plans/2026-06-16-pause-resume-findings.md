# Pause/Resume Durable Findings — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a `scan`/`mirror`/`repair` run be paused or killed without losing findings — checkpoint the `--report` JSON (and perf CSVs) periodically and on SIGINT/SIGTERM, and add an explicit `--resume` that merges prior findings.

**Architecture:** A new `Checkpointer` (src/checkpoint.rs) writes the report atomically (temp+rename), driven by the pipeline's completed-checkpoint loop (every N checkpoints + a 30 s time-backstop) and by a SIGINT/SIGTERM handler (hard exit after a final flush). `ArchiveStats` becomes `Arc`-shared so the signal handler and the pipeline see the same live findings. `--resume` loads the prior report and seeds the `FailureTracker` (union/preserve).

**Tech Stack:** Rust, tokio (multi-thread runtime, `tokio::signal`), serde / serde_json, clap.

**Source spec:** `docs/specs/2026-06-16-pause-resume-findings-design.md`. Read it first.

**Branch:** Implement on a dedicated branch off `perf-testing` (e.g. `feat/pause-resume`), NOT on `perf-verify-speedup` (that branch holds the unrelated verify perf experiment). Create it before Task 1.

**Conventions:** Build/test with `cargo`. Run perf-gated code/tests with `--features perf-metrics`. Each task is TDD: write the failing test, see it fail, implement, see it pass, commit.

---

## Milestone A — Report schema + atomic write (no behavior change)

### Task 1: Add `RunStatus` + `Progress` to the report (back-compat)

**Files:**
- Modify: `src/report.rs`
- Test: `src/report.rs` (inline `#[cfg(test)] mod`) or `src/tests/report_test.rs`

- [ ] **Step 1: Write the failing test** — add to `src/tests/report_test.rs`:

```rust
#[test]
fn report_run_status_and_progress_roundtrip_and_backcompat() {
    use stellar_archivist::report::{ArchiveReport, Progress, RunStatus, ReportSection, Summary, REPORT_VERSION};
    use std::collections::BTreeMap;

    // New fields round-trip.
    let r = ArchiveReport {
        version: REPORT_VERSION,
        run_status: RunStatus::Interrupted,
        progress: Progress { processed_checkpoints: 7, total_checkpoints: 10 },
        section: ReportSection {
            well_known: None, files: BTreeMap::new(), buckets: vec![], checkpoints: vec![],
            summary: Summary::default(),
        },
    };
    let json = serde_json::to_string(&r).unwrap();
    let back: ArchiveReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.run_status, RunStatus::Interrupted);
    assert_eq!(back.progress.processed_checkpoints, 7);

    // Old report WITHOUT the new fields still parses (defaults).
    let old = r#"{"version":1,"well_known":null,"files":{},"buckets":[],"checkpoints":[],
                  "summary":{"succeeded":1,"skipped":0,"failed":0,"retries":0}}"#;
    let parsed: ArchiveReport = serde_json::from_str(old).unwrap();
    assert_eq!(parsed.run_status, RunStatus::Complete); // default
    assert_eq!(parsed.progress.total_checkpoints, 0);   // default
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test '*' report_run_status_and_progress_roundtrip_and_backcompat 2>&1 | tail` (or `cargo test report_run_status`).
Expected: FAIL to compile — `RunStatus`/`Progress` and the fields don't exist.

- [ ] **Step 3: Implement** — in `src/report.rs`, add the types and fields:

```rust
/// Whether a report reflects a completed run or a paused/killed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Complete,
    Interrupted,
}

/// How far a run got (counts only — checkpoints complete out of order under
/// concurrency, and resume re-scans from the top, so no resume-point field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Progress {
    pub processed_checkpoints: u64,
    pub total_checkpoints: u64,
}
```

Add to `ArchiveReport` (after `version`, before `#[serde(flatten)] section`):

```rust
    #[serde(default)]
    pub run_status: RunStatus,
    #[serde(default)]
    pub progress: Progress,
```

Add the same two `#[serde(default)]` fields to `MultiSectionReport` (after `version`).

Then fix the two existing constructors so they compile: in `ArchiveReport::from_failures_and_summary`, set `run_status: RunStatus::Complete, progress: Progress::default(),`. Search the codebase for every `ArchiveReport {` and `MultiSectionReport {` literal (in `scan_operation.rs`, `repair_operation.rs`, tests) and add `run_status: RunStatus::Complete, progress: Progress::default(),` — these are the finalize paths, which are always "complete".

- [ ] **Step 4: Run tests** — `cargo test report_run_status` → PASS; `cargo build` → OK.

- [ ] **Step 5: Commit**

```bash
git add src/report.rs src/tests/report_test.rs src/scan_operation.rs src/repair_operation.rs
git commit -m "feat(report): add run_status + progress fields (serde-default, back-compat)"
```

### Task 2: Atomic report write (`write_to_path_atomic`)

**Files:**
- Modify: `src/report.rs`
- Test: `src/tests/report_test.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn write_to_path_atomic_writes_and_overwrites_without_leaving_tmp() {
    use stellar_archivist::report::{write_to_path_atomic, ArchiveReport, read_from_path};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.json");
    let r = ArchiveReport::from_failures_and_summary(&Default::default(), Default::default());
    write_to_path_atomic(&path, &r).unwrap();
    write_to_path_atomic(&path, &r).unwrap(); // overwrite is fine
    let _ = read_from_path(&path).unwrap();    // valid JSON
    assert!(!path.with_extension("json.tmp").exists()); // no stray tmp
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test write_to_path_atomic` → FAIL (fn missing).

- [ ] **Step 3: Implement** in `src/report.rs`:

```rust
/// Like [`write_to_path`] but atomic: serialize to `<path>.tmp`, fsync, then
/// rename over `<path>`. A crash mid-write never leaves a partial report.
pub fn write_to_path_atomic<T: Serialize>(path: &Path, report: &T) -> Result<(), ReportError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string_pretty(report)?;
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
```

- [ ] **Step 4: Run** — `cargo test write_to_path_atomic` → PASS.
- [ ] **Step 5: Commit**

```bash
git add src/report.rs src/tests/report_test.rs
git commit -m "feat(report): atomic write_to_path_atomic (temp+fsync+rename)"
```

---

## Milestone B — Checkpointer core (unit-tested in isolation)

### Task 3: `Checkpointer` with `flush_now` + `maybe_flush` (interval + backstop)

**Files:**
- Create: `src/checkpoint.rs`
- Modify: `src/lib.rs` (add `pub mod checkpoint;`)
- Test: `src/tests/checkpoint_test.rs` (+ register in the test module list, see existing `src/tests/mod.rs` pattern)

Design note: the render is operation-supplied so all three ops reuse one mechanism. It takes the status+progress and returns the report as a `serde_json::Value`. The Checkpointer owns timing, the flush lock, and the atomic write. `Instant`/`Duration` are runtime-only (fine here — this is not a workflow script).

- [ ] **Step 1: Write the failing test**

```rust
use stellar_archivist::checkpoint::{Checkpointer, RenderFn};
use stellar_archivist::report::{Progress, RunStatus};
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
    assert_eq!(v["run_status"], "complete");        // periodic flush is "complete"? -> see note
    assert_eq!(v["progress"]["total_checkpoints"], 10);

    cp.flush_now(RunStatus::Interrupted).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(v["run_status"], "interrupted");
}
```

Note: periodic flushes use `RunStatus::Interrupted` too (the run isn't complete until `finalize`). Adjust the assertion to `"interrupted"` for the interval write — `maybe_flush` always flushes with `Interrupted`; only `finalize` writes `Complete`. (Fix the test's first assertion accordingly before running.)

- [ ] **Step 2: Run to verify it fails** — `cargo test --test '*' maybe_flush_triggers 2>&1 | tail` → FAIL (module missing).

- [ ] **Step 3: Implement** `src/checkpoint.rs`:

```rust
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
pub type RenderFn =
    std::sync::Arc<dyn Fn(RunStatus, Progress) -> BoxFuture<'static, Result<serde_json::Value, ReportError>> + Send + Sync>;

pub struct Checkpointer {
    report_path: PathBuf,
    interval: usize,         // flush every N completed checkpoints (0 = off)
    backstop: Duration,      // also flush if >= this since last write
    total: AtomicU64,        // total checkpoints (for Progress); set via set_total
    render: RenderFn,
    last_completed: AtomicU64,
    inner: Mutex<Inner>,     // serializes writes + holds last-flush instant
}

struct Inner {
    last_flush_at: Instant,
}

impl Checkpointer {
    pub fn new(report_path: PathBuf, interval: usize, backstop: Duration, total: u64, render: RenderFn) -> Self {
        Self {
            report_path,
            interval,
            backstop,
            total: AtomicU64::new(total),
            render,
            last_completed: AtomicU64::new(0),
            inner: Mutex::new(Inner { last_flush_at: Instant::now() }),
        }
    }

    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    /// Periodic driver: flush if the interval was crossed or the time backstop
    /// elapsed. Cheap and safe to call from every checkpoint completion.
    pub async fn maybe_flush(&self, completed: usize) {
        let due_by_count = self.interval > 0
            && completed as u64 >= self.last_completed.load(Ordering::Relaxed) + self.interval as u64;
        // Time backstop checked under the lock (needs last_flush_at).
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

    /// Force a flush (signal handler). `completed` defaults to last seen.
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
            tracing::warn!("checkpoint write failed for {}: {e}", self.report_path.display());
            return Err(e); // caller ignores in periodic path
        }
        inner.last_flush_at = Instant::now();
        Ok(())
    }
}
```

Add `pub mod checkpoint;` to `src/lib.rs` and register `mod checkpoint_test;` in `src/tests/mod.rs`.

- [ ] **Step 4: Run** — fix the test's first assertion to `"interrupted"`, then `cargo test maybe_flush_triggers` → PASS.
- [ ] **Step 5: Commit**

```bash
git add src/checkpoint.rs src/lib.rs src/tests/checkpoint_test.rs src/tests/mod.rs
git commit -m "feat(checkpoint): Checkpointer with interval + time-backstop flush"
```

### Task 4: `Checkpointer::load` + `FailureTracker` union (for --resume)

**Files:**
- Modify: `src/checkpoint.rs` (add `load`)
- Modify: `src/utils.rs` (add `FailureTracker::union_from`)
- Test: `src/tests/checkpoint_test.rs`, `src/tests/utils` (or inline)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn failure_tracker_union_preserves_and_merges() {
    use stellar_archivist::utils::FailureTracker;
    let mut a = FailureTracker::default();
    a.record_checkpoint(127);
    a.record_file(63, stellar_archivist::utils::FileFlags::LEDGER);
    let mut b = FailureTracker::default();
    b.record_checkpoint(191);                       // new
    b.record_file(63, stellar_archivist::utils::FileFlags::RESULTS); // merges into cp 63
    a.union_from(&b);
    assert!(a.checkpoints.contains(&127) && a.checkpoints.contains(&191)); // preserved + merged
    assert!(a.files.get(&63).unwrap().has(stellar_archivist::utils::FileFlags::LEDGER));
    assert!(a.files.get(&63).unwrap().has(stellar_archivist::utils::FileFlags::RESULTS));
}

#[test]
fn checkpointer_load_reads_prior_failures() {
    use stellar_archivist::report::{ArchiveReport, write_to_path_atomic};
    use stellar_archivist::checkpoint::Checkpointer;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.json");
    let mut t = stellar_archivist::utils::FailureTracker::default();
    t.record_checkpoint(127);
    write_to_path_atomic(&path, &ArchiveReport::from_failures_and_summary(&t, Default::default())).unwrap();
    let loaded = Checkpointer::load(&path).unwrap();
    assert!(loaded.checkpoints.contains(&127));
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test failure_tracker_union checkpointer_load` → FAIL.

- [ ] **Step 3: Implement.** In `src/utils.rs` on `impl FailureTracker`:

```rust
/// Union another tracker's failures into this one (preserve + merge; never
/// clears). Used by --resume to seed prior findings.
pub fn union_from(&mut self, other: &FailureTracker) {
    if other.well_known.is_some() { self.well_known = other.well_known; }
    for (&cp, &flags) in &other.files {
        for bit in [FileFlags::HISTORY, FileFlags::LEDGER, FileFlags::TRANSACTIONS, FileFlags::RESULTS, FileFlags::SCP] {
            if flags.has(bit) { self.record_file(cp, bit); }
        }
    }
    for h in &other.buckets { self.buckets.insert(*h); }
    for &cp in &other.checkpoints { self.checkpoints.insert(cp); }
}
```

In `src/checkpoint.rs`:

```rust
impl Checkpointer {
    /// Load prior findings from an existing single-section report (for --resume).
    /// Hard error on a missing/unparseable/unsupported report — never guesses.
    pub fn load(path: &std::path::Path) -> Result<crate::utils::FailureTracker, ReportError> {
        let report = crate::report::read_from_path(path)?;
        report.into_failures()
    }
}
```

- [ ] **Step 4: Run** — `cargo test failure_tracker_union checkpointer_load` → PASS.
- [ ] **Step 5: Commit**

```bash
git add src/checkpoint.rs src/utils.rs src/tests/checkpoint_test.rs
git commit -m "feat(checkpoint): load() + FailureTracker::union_from for --resume"
```

---

## Milestone C — Share stats + wire periodic flush into the pipeline

### Task 5: Make `ArchiveStats` Arc-shared in the pipeline

**Files:** Modify `src/pipeline.rs` (`Pipeline.stats: Arc<ArchiveStats>`, constructor, `stats()`, `into_stats`), and any caller that breaks (`repair_operation.rs`).

Rationale: the signal handler (Task 7) must reach the same live findings as the pipeline, so stats must be shareable.

- [ ] **Step 1: Write the failing test** — compile-driver test in `src/tests/pipeline_test.rs`:

```rust
#[test]
fn pipeline_exposes_arc_stats() {
    // Type-level assertion: into_stats returns Arc<ArchiveStats>.
    fn _assert(p: stellar_archivist::pipeline::Pipeline<stellar_archivist::scan_operation::ScanOperation>) {
        let _: std::sync::Arc<stellar_archivist::utils::ArchiveStats> = p.into_stats();
    }
}
```

- [ ] **Step 2: Run** — `cargo build --tests` → FAIL (into_stats returns by value today).

- [ ] **Step 3: Implement** — in `src/pipeline.rs`:
  - `stats: std::sync::Arc<ArchiveStats>` in the struct; `Pipeline::new` builds `Arc::new(ArchiveStats::new())`.
  - `pub fn stats(&self) -> &ArchiveStats { &self.stats }` (deref still works).
  - `pub fn into_stats(self) -> Arc<ArchiveStats> { self.stats }`.
  - Update `repair_operation.rs` callers of `into_stats()` to accept `Arc<ArchiveStats>` (use `&*` / `Arc::try_unwrap(...).unwrap_or_else(|a| (*a).clone())` only if an owned value is truly needed — prefer reading via `&`).

- [ ] **Step 4: Run** — `cargo build --tests` → OK; `cargo test pipeline_exposes_arc_stats` → PASS; `cargo test` (repair tests) → PASS.
- [ ] **Step 5: Commit**

```bash
git add src/pipeline.rs src/repair_operation.rs src/tests/pipeline_test.rs
git commit -m "refactor(pipeline): Arc-share ArchiveStats for checkpointing"
```

### Task 6: Drive periodic flush from `run_checkpoints`

**Files:** Modify `src/pipeline.rs` (hold `Option<Arc<Checkpointer>>`, call `maybe_flush`), `src/cli/{scan,mirror,repair}.rs` + the operations to build the Checkpointer and pass it in.

- [ ] **Step 1: Write the failing test** — integration in `src/tests/checkpoint_test.rs` (uses the test fixture server pattern already in `src/tests/utils.rs`; or a local `file://` temp archive). Assert that after a bounded scan with `--report` + interval=1, the report exists and has `progress.total_checkpoints == <n>` and `run_status == complete` (finalize wins at the end). Then assert that a *periodic* write happened by setting interval=1 and checking the file is created before finalize — simplest reliable check: run the scan, confirm the report parses and `progress.processed_checkpoints == total_checkpoints`.

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scan_writes_progress_in_report() {
    // build_local_test_archive(...) -> returns a file:// URL with N checkpoints
    let (url, n) = stellar_archivist::tests::utils::tiny_local_archive().await;
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("r.json");
    stellar_archivist::cli::run([
        "stellar-archivist","scan",&url,"--report",report.to_str().unwrap(),
        "--checkpoint-interval","1",
    ]).await.unwrap();
    let v = stellar_archivist::report::read_from_path(&report).unwrap();
    assert_eq!(v.progress.total_checkpoints, n as u64);
    assert_eq!(v.run_status, stellar_archivist::report::RunStatus::Complete);
}
```

(If no `tiny_local_archive` helper exists, add one to `src/tests/utils.rs` that materializes `testdata/testnet-archive-small` as a `file://` URL — follow the existing fixture helpers there.)

- [ ] **Step 2: Run** — FAIL (flag `--checkpoint-interval` unknown; no progress set).

- [ ] **Step 3: Implement:**
  - Add to `Cli` (src/cli/mod.rs): `#[arg(long, global = true, default_value_t = 200)] checkpoint_interval: usize` and `#[arg(long, global = true)] resume: bool`. Add `checkpoint_interval: usize` and `resume: bool` to `GlobalArgs`; populate in `run`.
  - `Pipeline` gains `checkpointer: Option<Arc<Checkpointer>>` (new constructor arg, or a `with_checkpointer` setter). In `run()`, after computing `total_count`, call `if let Some(cp) = &self.checkpointer { cp.set_total(total_count as u64); }`.
  - In `run_checkpoints`, inside the `for_each_concurrent` completion closure (right after `let done = ...`), add: `if let Some(cp) = &self.checkpointer { cp.maybe_flush(done).await; }`.
  - Build the Checkpointer in each operation's CLI runner (scan/mirror/repair) when `report_path.is_some()`: construct the `RenderFn` capturing `Arc<ArchiveStats>` (from the pipeline — so build stats first, or expose `pipeline.stats_arc()`), e.g. for scan:

```rust
let render: RenderFn = {
    let stats = pipeline.stats_arc(); // add: pub fn stats_arc(&self) -> Arc<ArchiveStats>
    std::sync::Arc::new(move |status, progress| {
        let stats = stats.clone();
        Box::pin(async move {
            let section = stats.report_section().await;
            serde_json::to_value(crate::report::ArchiveReport {
                version: crate::report::REPORT_VERSION,
                run_status: status,
                progress,
                section,
            }).map_err(crate::report::ReportError::from)
        })
    })
};
let checkpointer = Arc::new(Checkpointer::new(
    report_path.clone(), args.checkpoint_interval, std::time::Duration::from_secs(30), 0, render));
pipeline.set_checkpointer(checkpointer.clone());
```

  - `repair`'s render renders its current main-pass stats the same way (single section). Document that mid-run repair snapshots show the current pass; the final multi-section is written by `finalize`.

- [ ] **Step 4: Run** — `cargo test scan_writes_progress_in_report` → PASS; `cargo test` → PASS.
- [ ] **Step 5: Commit**

```bash
git add src/pipeline.rs src/cli/ src/scan_operation.rs src/mirror_operation.rs src/repair_operation.rs src/tests/
git commit -m "feat(checkpoint): periodic report flush from the pipeline loop + CLI flags"
```

---

## Milestone D — Signal handling (hard exit after flush)

### Task 7: SIGINT/SIGTERM handler that flushes then hard-exits

**Files:** Modify `src/cli/mod.rs` (spawn the signal task in `run`, after building the checkpointer; needs the `Arc<Checkpointer>` shared out of the operation runner) and/or `src/bin/stellar-archivist/main.rs`.

Design: the operation runner builds the `Arc<Checkpointer>`. To let `cli::run` spawn the signal task, have each command's `run` return early-construct the checkpointer, OR move signal spawning into the command runners right after the checkpointer exists. Recommended: a small helper `checkpoint::spawn_signal_handler(cp: Arc<Checkpointer>)` called from each runner once the checkpointer is built.

- [ ] **Step 1: Write the failing integration test** — `src/tests/checkpoint_test.rs`. Spawn the binary as a subprocess (use `assert_cmd` or `std::process::Command` on the built debug binary) scanning a slow/large-enough local archive with `--report` + `--checkpoint-interval 1`, send SIGINT after it starts, then assert the report exists with `run_status == interrupted`. If a subprocess test is too heavy for CI, instead unit-test the handler logic by calling `checkpointer.flush_now(RunStatus::Interrupted)` directly (Task 3 already covers the flush) and gate the subprocess test behind `#[ignore]` with a comment to run manually.

```rust
#[tokio::test(flavor = "multi_thread")]
#[ignore = "subprocess + signal; run manually: cargo test -- --ignored sigint"]
async fn sigint_writes_interrupted_report() {
    // 1. spawn target/debug/stellar-archivist scan <big local file://> --report r.json --checkpoint-interval 1
    // 2. wait until r.json appears (first periodic flush) OR ~2s
    // 3. send SIGINT (nix::sys::signal::kill) to the child
    // 4. wait for exit; assert r.json parses with run_status == Interrupted
}
```

- [ ] **Step 2: Run** — `cargo build` (handler not yet wired) — the `#[ignore]` test compiles; the real assertion is manual.

- [ ] **Step 3: Implement** in `src/checkpoint.rs`:

```rust
use std::sync::Arc;

/// Install a SIGINT/SIGTERM handler that does one final flush then hard-exits.
/// First signal: flush(Interrupted) -> process::exit(130). Second: exit now.
pub fn spawn_signal_handler(cp: Arc<Checkpointer>) {
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        tracing::warn!("signal received — flushing report and exiting");
        // Race a second signal: if it arrives, exit immediately.
        tokio::select! {
            r = cp.flush_now(RunStatus::Interrupted) => {
                if let Err(e) = r { tracing::error!("final flush failed: {e}"); }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::warn!("second signal — exiting without finishing flush");
            }
        }
        std::process::exit(130);
    });
}
```

Call `checkpoint::spawn_signal_handler(checkpointer.clone())` in each command runner right after the checkpointer is built (Task 6). Guard for non-unix if the project must build on Windows: `#[cfg(unix)]` around the SIGTERM arm, falling back to `ctrl_c()` only. (Confirm target platforms; the repo's `opendal-unix` feature suggests unix-first.)

- [ ] **Step 4: Verify manually** — `cargo build`; run `cargo test -- --ignored sigint` or manually: start a scan on the big local fixture with `--checkpoint-interval 1`, Ctrl-C, inspect the report (`run_status: "interrupted"`). Confirm a second Ctrl-C exits instantly.
- [ ] **Step 5: Commit**

```bash
git add src/checkpoint.rs src/cli/
git commit -m "feat(checkpoint): SIGINT/SIGTERM handler — flush then hard exit"
```

---

## Milestone E — `--resume` seeding

### Task 8: Seed prior findings on `--resume`

**Files:** Modify the operation constructors / pipeline so a resumed run seeds `FailureTracker` from the prior report; warn on range mismatch and on overwriting an `interrupted` report without `--resume`.

- [ ] **Step 1: Write the failing test** — `src/tests/checkpoint_test.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_unions_prior_findings() {
    // 1. Write a prior report with a broken checkpoint that the current scan won't re-encounter
    //    (e.g. a checkpoint outside --low/--high), run_status interrupted.
    // 2. Run scan with --resume --report <that path> over a healthy range.
    // 3. Assert the final report still contains the prior broken checkpoint (preserved/union).
}
```

- [ ] **Step 2: Run** — FAIL (resume not wired).

- [ ] **Step 3: Implement:**
  - In each command runner, when `args.resume`, before `pipeline.run()`: `let prior = Checkpointer::load(report_path)?;` then seed the pipeline's stats: add `pub async fn seed_failures(&self, prior: FailureTracker)` on `ArchiveStats` that does `self.failures.lock().await.union_from(&prior)`, and call `pipeline.stats().seed_failures(prior).await`.
  - Without `--resume`: if `report_path` exists and parses with `run_status == Interrupted`, log a warning (“overwriting an interrupted report; pass --resume to keep its findings”).
  - On `--resume` parse error → return `Err` (clear message) — do not start fresh.
  - Range mismatch warning: compare the loaded report's coverage if available; if not tracked, skip (document as best-effort).

- [ ] **Step 4: Run** — `cargo test resume_unions_prior_findings` → PASS.
- [ ] **Step 5: Commit**

```bash
git add src/cli/ src/utils.rs src/tests/checkpoint_test.rs
git commit -m "feat(checkpoint): --resume seeds prior findings (union/preserve)"
```

---

## Milestone F — perf-metrics snapshot durability (feature-gated)

### Task 9: `metrics::snapshot()` + Checkpointer hook + resume CSV rename

**Files:** Modify `src/metrics.rs` (factor phases/headline writing into a helper; add `snapshot(elapsed)` that does NOT stop the sampler), `src/checkpoint.rs` (call it from `flush` under `cfg(feature="perf-metrics")`), and the resume path (rename existing CSVs).

- [ ] **Step 1: Write the failing test** (feature-gated) — `src/tests/checkpoint_test.rs`:

```rust
#[cfg(feature = "perf-metrics")]
#[test]
fn metrics_snapshot_writes_csvs() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SA_PERF_OUT", dir.path());
    stellar_archivist::metrics::snapshot(std::time::Duration::from_secs(1));
    assert!(dir.path().join("phases.csv").exists());
    assert!(dir.path().join("headline.csv").exists());
}
```

- [ ] **Step 2: Run** — `cargo test --features perf-metrics metrics_snapshot_writes_csvs` → FAIL (fn missing).

- [ ] **Step 3: Implement** in `src/metrics.rs`:
  - Extract the phases.csv + headline.csv writing from `report(wall)` into `fn write_csvs(wall: Duration)`.
  - `report(wall)` keeps `SAMPLER_STOP.store(true,...)` + eprintln summary + `write_csvs(wall)`.
  - Add `pub fn snapshot(elapsed: Duration) { write_csvs(elapsed); }` — does NOT set `SAMPLER_STOP` (sampler keeps running). Export `snapshot` from the `imp` re-export and add a no-op `snapshot` in the `#[cfg(not(feature="perf-metrics"))]` shim.
  - In `Checkpointer::flush`, after the report write: `#[cfg(feature = "perf-metrics")] crate::metrics::snapshot(self.started.elapsed());` — add a `started: Instant` to Checkpointer set at `new()`.
  - Resume CSV rename: in the `--resume` path, before the run, if `SA_PERF_OUT/phases.csv` exists, rename existing `phases.csv`/`headline.csv`/`timeseries.csv` to `*.prev-<n>.csv`. (Small helper, feature-gated.)

- [ ] **Step 4: Run** — `cargo test --features perf-metrics metrics_snapshot_writes_csvs` → PASS; `cargo build` (no feature) → OK (no-op shim).
- [ ] **Step 5: Commit**

```bash
git add src/metrics.rs src/checkpoint.rs src/cli/ src/tests/checkpoint_test.rs
git commit -m "feat(metrics): snapshot() for checkpoint durability (feature-gated)"
```

---

## Milestone G — repair integration + docs

### Task 10: Repair mid-run snapshot + final multi-section coexist

**Files:** `src/tests/repair_op_test.rs` (or checkpoint_test), `docs/` user docs.

- [ ] **Step 1: Write the test** — run a repair (with `--report`, interval=1) over a corrupted local copy; assert it completes with a valid `MultiSectionReport` (final), and that an interim single-section snapshot is valid JSON (parse it mid-run is hard in-process — instead assert the final report is a valid `MultiSectionReport` and that a forced `flush_now` during the main pass produced a parseable single-section report; this can be a direct Checkpointer test using repair's render).
- [ ] **Step 2: Run** → FAIL/iterate.
- [ ] **Step 3: Implement** any fixes needed in repair's render wiring (Task 6 already builds it; ensure repair’s runner builds the checkpointer + signal handler like scan/mirror).
- [ ] **Step 4: Run** → PASS; full `cargo test` → PASS; `cargo test --features perf-metrics` → PASS.
- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "test(checkpoint): repair mid-run snapshot + final multi-section"
```

### Task 11: User docs + CHANGELOG

- [ ] Add a short section to `README.md` (and/or a `docs/` page): `--resume`, `--checkpoint-interval`, periodic + Ctrl-C behavior (hard exit after flush, interrupted vs complete), the union/preserve resume semantics, and the perf-snapshot note. Commit.

---

## Self-review notes (gaps to watch during execution)

- **Spec deviation (flag to user):** mid-run *repair* snapshots are **single-section** (current pass), not the full `MultiSectionReport` — the multi-section is written only at `finalize`. The spec described an operation-supplied multi-section render mid-run; this plan renders the current pass's section for simplicity. Functionally equivalent for "don't lose findings." Confirm acceptable.
- **Platform:** signal handling uses `tokio::signal::unix` for SIGTERM; wrap in `#[cfg(unix)]` and confirm whether Windows must be supported (repo has `opendal-unix`, suggests unix-first).
- **Counts on resume:** `succeeded`/`skipped` reflect the new run; failure *sets* union (per spec §5).
- **`tiny_local_archive` test helper** may need adding to `src/tests/utils.rs` if absent.
- **No-`--report` signal behavior (spec §6):** when `--report` is absent there is no Checkpointer, so no signal handler is installed and Ctrl-C kills with the default behavior. The spec asks for a stderr summary dump in that case — minor; either install a summary-only signal handler when `report_path` is None, or accept the default and drop the spec line. Decide during Task 7 (low priority).
- **Signal/subprocess tests** (Tasks 7, 8, 10) are described as comment-scaffolded / `#[ignore]` because they need real signals or a child process; the deterministic core (flush, load, union, interval/backstop) is fully unit-tested in Tasks 2–4. Flesh out the subprocess harness during execution.
