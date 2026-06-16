# Design: Pause/Resume — durable findings for archivist runs

**Date:** 2026-06-16
**Status:** Approved design, ready for implementation planning
**Component:** `stellar-archivist` (scan / mirror / repair)

## 1. Problem & goal

A long archivist run (e.g. a full-pubnet `scan --verify`, many hours) writes its
report **only at the end** (`operation.finalize`). There is **no signal
handling**, so a Ctrl-C, SIGTERM, OOM, or crash kills the process and **loses
all findings** discovered so far — broken files, broken checkpoints, broken
buckets, well-known status, counts, and the `perf-metrics` timing.

**Goal:** make findings **durable** — a run can be paused or killed and its
findings survive on disk, and a later run can optionally continue without losing
the earlier findings.

### In scope
- Periodic + on-signal checkpointing of the `--report` JSON for `scan`, `mirror`,
  and `repair`.
- Explicit `--resume` that merges prior findings from an existing report.
- Durability snapshots of the `perf-metrics` CSVs (feature-gated).

### Non-goals (explicit)
- **Skipping already-completed work on resume.** A resumed run **re-scans from
  the top**; it does not persist a per-checkpoint "done" set. (Re-scanning is
  idempotent and reproduces findings; true progress-resume is a possible future
  follow-up.)
- **Cross-run perf accumulation.** Perf counters start fresh each run; wall-clock
  across a paused gap is intentionally not modeled.
- **Clearing stale findings.** Resume *unions* findings (preserve). A finding
  from run 1 that is healthy in run 2 is **not** cleared; resume assumes the
  archive is unchanged between runs.

## 2. Approach (chosen: a dedicated `Checkpointer`)

A new, well-bounded `src/checkpoint.rs` component owns all checkpoint logic, so
the feature is isolated and unit-testable, and the periodic and signal paths
share one flush implementation. Two alternatives were considered and rejected:
inlining into the pipeline + `Operation` trait (scatters logic across the
pipeline and every `finalize`), and a detached time-based background task
(time-only, awkward `Arc` sharing, still needs the same flush code).

### 2.1 The `Checkpointer`

```
Checkpointer
  owns:   report_path: PathBuf
          interval: usize                 // flush every N completed checkpoints (0 = off)
          time_backstop: Duration         // also flush if >= this since last write (30 s)
          stats: Arc<ArchiveStats>
          render: <async callback> -> serde_json::Value   // operation-supplied
          flush_lock: tokio::sync::Mutex<()>              // serialize writes
          last_flush_completed: AtomicUsize
          last_flush_at: <monotonic instant, behind the lock>
  api:    async maybe_flush(completed: usize)   // periodic driver (pipeline loop)
          async flush_now(status: RunStatus)    // signal driver
          load(path) -> PriorFindings           // for --resume
```

- **Render callback** is operation-supplied so the Checkpointer never knows
  operation specifics: `scan`/`mirror` render an `ArchiveReport` from one
  `ArchiveStats::snapshot_section()`; `repair` renders its `MultiSectionReport`
  from its cumulative section accumulator. Both serialize to `serde_json::Value`.
  The render produces only the *findings* part; the **Checkpointer injects
  `run_status` and `progress`** (which it owns — it knows the completed count and
  the caller's status) into the top level of the rendered value before writing.
- **Atomic write:** serialize to `<report>.tmp`, `fsync`, `rename` over
  `<report>` (matches the storage layer's existing temp+rename pattern), so a
  kill mid-write never corrupts the report.

## 3. Data flow: periodic + signal

### 3.1 Periodic (progress-driven)
`pipeline.rs::run_checkpoints` already counts completed checkpoints in its
`for_each_concurrent` completion closure. After each completion it calls
`checkpointer.maybe_flush(done)`. `maybe_flush` flushes when **either**
`done - last_flush_completed >= interval` **or** `now - last_flush_at >=
time_backstop`. An atomic check + the `flush_lock` ensure exactly one task writes
per trigger (no overlapping writes from concurrent completions). The time
backstop guarantees progress even when a few huge checkpoints (e.g. the 2.36 GB
bucket) span many minutes.

Defaults: `interval = 200` checkpoints, `time_backstop = 30 s`.

### 3.2 Signal (hard exit after flush)
At run start, spawn a `tokio::signal` task watching **SIGINT + SIGTERM**:
```
first signal  -> checkpointer.flush_now(Interrupted)  -> std::process::exit(130)
second Ctrl-C -> std::process::exit(130) immediately   (never hang)
```
"Hard exit" = after the final atomic flush completes, the process stops via
`process::exit` without draining the runtime or finishing in-flight checkpoints.
Only *completed-and-recorded* findings were ever in the report, so the on-disk
report is consistent as of the last fully-processed checkpoint. Side effect: a
mirror/repair write that was streaming to a `<file>.tmp` is orphaned (harmless
scratch, ignored/overwritten next run) — noted for a cleanup pass on resume.

Both periodic and signal go through the same `flush()` (render → atomic write),
serialized by `flush_lock`.

## 4. Report schema changes

Add two **backward-compatible** (`#[serde(default)]`) fields to the report
(`report.rs`), so old reports still parse:
- `run_status: "complete" | "interrupted"` — distinguishes a partial report.
- `progress: { processed_checkpoints, total_checkpoints }` — how far the run got.
  (Counts only — checkpoints complete out of order under concurrency, and resume
  re-scans from the top, so no "resume point" field is needed.)

A periodic/signal flush writes `run_status: "interrupted"`; `finalize` writes
`"complete"`. For `repair`'s `MultiSectionReport`, the fields live at the
top level of the multi-section envelope.

## 5. `--resume` semantics

`--resume` is only meaningful with `--report`.
- **Load + seed:** parse the existing report; seed the `FailureTracker` with its
  prior broken sets (buckets / checkpoints / per-cp file-flag bitmask /
  well-known). Then run normally.
- **Union/preserve:** new findings union into the seeded sets (`BTreeSet` →
  idempotent). A prior finding not re-checked this run is preserved. Stale
  findings are not cleared (non-goal §1).
- **Counts:** `succeeded`/`skipped` reflect the **new run's** scope (re-scan from
  top); failure *sets* are the union. The report flips to `complete` when the new
  run finishes its full range.
- **Errors, not guesses:** a missing or unparseable `--resume` report is a hard
  error with a clear message (never silently start fresh).
- **Range-mismatch warning:** if the loaded report's range differs from the
  current `--low/--high`, warn (findings still union).
- **No-resume overwrite guard:** without `--resume`, a run starts fresh and
  overwrites `--report`; but if the existing file is `run_status: interrupted`,
  warn first so a paused run's findings are not silently discarded.

## 6. CLI flags (global, only meaningful with `--report`)

- `--resume` (bool, default false) — load `--report` and seed/merge prior findings.
- `--checkpoint-interval <N>` (default **200**) — flush every N completed
  checkpoints. `0` disables periodic flushing (signal-only).
- Time backstop (**30 s**) is built in (flush if that long since the last write);
  not a user knob in v1.

Without `--report`, checkpointing is inert; a signal prints the run summary to
stderr and exits.

## 7. perf-metrics snapshot durability (feature-gated)

- Add `metrics::snapshot()` — writes `phases.csv` + `headline.csv` from current
  counters, using elapsed-so-far as wall. (`timeseries.csv` is already written
  every 2 s and is already durable.)
- When `perf-metrics` is on, `Checkpointer::flush()` also calls
  `metrics::snapshot()`, so periodic and signal flushes carry timing-so-far.
- On `--resume`: perf starts fresh (no accumulation, §1). Before the new run
  writes, any existing `phases.csv` / `headline.csv` are renamed with a timestamp
  suffix so prior timing is kept, not clobbered.

## 8. Repair specifics

`repair` is the only op with a `MultiSectionReport` (a `main_pass` section + one
section per retry pass) driven by multiple internal pipelines.
- Its render callback produces the cumulative `MultiSectionReport` from repair's
  current section accumulator, so periodic/signal flushes work during the long
  main pass and the short retry passes.
- `--resume` for repair: seed prior **failure findings** and re-run; the
  multi-section *pass breakdown* resets for the new run (prior findings preserved
  via the union, not by replaying old passes). Durability is full; resume =
  "don't lose what was found, re-run to finish."

## 9. Error handling & edge cases

- Atomic-write failure → log + continue (a checkpoint-write hiccup must never
  kill the run).
- Periodic vs signal flush overlap → serialized by `flush_lock`.
- `--resume` on corrupt/old/missing report → hard error, clear message.
- `--resume` range ≠ current `--low/--high` → warn; findings still union.
- Double Ctrl-C → immediate hard exit (no hang).
- No `--report` → feature inert; signal dumps summary to stderr, exits.

## 10. Testing

- **Unit:** atomic write (tmp+rename); `Checkpointer::load()` round-trip;
  `FailureTracker` union from a loaded report; `maybe_flush` interval + time-
  backstop triggering; report serde back-compat (old report without the new
  fields parses).
- **Integration:** scan a local fixture, send SIGINT mid-run → assert report
  exists, valid, `run_status: interrupted`, holds findings-so-far; then
  `--resume` → final report `complete`, findings ⊇ partial. Simulated hard kill
  between flushes → last snapshot valid (atomic-write guarantee). Repair →
  valid `MultiSectionReport` mid-run.
- **Feature-gated:** `metrics::snapshot()` writes valid CSVs mid-run.

## 11. Touch list (anticipated)

- `src/checkpoint.rs` (new) — `Checkpointer`, atomic write, `load()`.
- `src/report.rs` — `run_status` + `progress` fields (serde default); a
  `serde_json::Value` render helper.
- `src/utils.rs` — `FailureTracker` seed-from-prior (union) helper.
- `src/pipeline.rs` — `maybe_flush` call in the completion closure; thread the
  Checkpointer + render through.
- `src/cli/mod.rs` + `src/cli/{scan,mirror,repair}.rs` — flags; build the
  Checkpointer with the op's render; spawn the signal task; pass resume seed.
- `src/metrics.rs` — `snapshot()` + resume CSV rename (feature-gated).
- Tests under `src/tests/`.
