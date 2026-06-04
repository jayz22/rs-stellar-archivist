# Perf Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the feature-gated in-process instrumentation, the `corrupt-archive` tool, and the `scripts/perf/` harness that make `docs/perf-testing-plan.md` executable.

**Architecture:** All perf code is gated behind a new `perf-metrics` cargo feature (off by default → compiled to no-ops → production/PR builds unaffected). A `metrics` module aggregates per-phase timing via atomics; RAII guards are inserted at hot-path boundaries. A separate `corrupt-archive` binary reuses the proven test corruption helpers. Shell scripts wrap runs with OS `time` + a `ps` RSS sampler and drive the `-C` sweep; a Python script plots CSV.

**Tech Stack:** Rust (libc for `getrusage`, walkdir+rand for the corruption bin, flate2/sha2/stellar-xdr for re-encoding), bash, Python+matplotlib.

**Branch:** all work on `perf-testing` (off `repair-v2`). Nothing here is merged into the repair PR.

**Plan location note:** the runnable test plan is `docs/perf-testing-plan.md`; this is its build plan.

---

## File structure

| File | Responsibility |
|---|---|
| `Cargo.toml` (modify) | add `perf-metrics` feature + optional deps (`libc`,`walkdir`,`rand`); add `[[bin]] corrupt-archive`. |
| `src/metrics.rs` (create) | Phase enum, atomic counters, RAII `Guard`, getrusage peak RSS, report(), timeseries sampler. No-ops when feature off. |
| `src/lib.rs` (modify) | `pub mod metrics;` + the `phase!` macro (two cfg variants). |
| `src/bin/stellar-archivist/main.rs` (modify) | wall timer + sampler start + `metrics::report()` (feature-gated), before exit. |
| `src/verify.rs`, `src/xdr_verify.rs`, `src/pipeline.rs`, `src/storage.rs` (modify) | insert `phase!` guards + `metrics::add_bytes`/`record_file` at the mapped boundaries. |
| `src/corruption.rs` (create) | ported ledger helpers + the full corruption menu + `Manifest`. Gated by `perf-metrics`. |
| `src/bin/corrupt-archive/main.rs` (create) | CLI for the corruption tool. |
| `scripts/perf/run.sh`, `scaling.sh`, `plot.py`, `env.sh` (create) | measurement harness. |
| `.gitignore` (modify) | ignore `perf-results/` and `bin/`. |

---

## Task 1: Branch + Cargo wiring + empty metrics module

**Files:** Modify `Cargo.toml`, `src/lib.rs`; Create `src/metrics.rs`.

- [ ] **Step 1: Create the branch**

```bash
git switch -c perf-testing repair-v2
git switch -c perf-testing 2>/dev/null; git rev-parse --abbrev-ref HEAD   # confirm: perf-testing
```

- [ ] **Step 2: Add feature + optional deps to `Cargo.toml`**

In `[features]` (after the `cli = …` line) add:
```toml
# Perf testing only (NOT for production; off by default, compiled out otherwise).
perf-metrics = ["dep:libc"]                       # in-process instrumentation
corruption-tool = ["dep:walkdir", "dep:rand"]     # the corrupt-archive bin
```
In `[dependencies]` add:
```toml
libc = { version = "0.2", optional = true }
walkdir = { version = "2", optional = true }
rand = { version = "0.8", optional = true }
```
(`walkdir`/`rand` already exist under `[dev-dependencies]`; cargo unifies the versions.)

- [ ] **Step 3: Create `src/metrics.rs` with the Phase enum + public no-op surface**

```rust
//! Lightweight, feature-gated performance instrumentation.
//!
//! Compiled to no-ops unless built with `--features perf-metrics`. Phase timers
//! aggregate across concurrent tasks via atomics. See docs/perf-testing-plan.md §2.
//! Phase times overlap (concurrency + spawned tasks) and are a profiler-style
//! self-time breakdown, not a wall-clock critical path.

#[derive(Clone, Copy, Debug)]
pub enum Phase {
    HistoryFetch,
    HistoryParse,
    BucketStream,
    XdrDecompress,
    XdrParseLedger,
    XdrParseTx,
    XdrParseResult,
    XdrParseScp,
    CrossFileVerify,
    ChainVerify,
    Copy,
}

impl Phase {
    pub const COUNT: usize = 11;
    pub fn name(self) -> &'static str {
        match self {
            Phase::HistoryFetch => "history_fetch",
            Phase::HistoryParse => "history_parse",
            Phase::BucketStream => "bucket_stream",
            Phase::XdrDecompress => "xdr_decompress",
            Phase::XdrParseLedger => "xdr_parse_ledger",
            Phase::XdrParseTx => "xdr_parse_tx",
            Phase::XdrParseResult => "xdr_parse_result",
            Phase::XdrParseScp => "xdr_parse_scp",
            Phase::CrossFileVerify => "cross_file_verify",
            Phase::ChainVerify => "chain_verify",
            Phase::Copy => "copy",
        }
    }
}

#[cfg(not(feature = "perf-metrics"))]
#[inline(always)]
pub fn add_bytes(_p: Phase, _n: u64) {}
#[cfg(not(feature = "perf-metrics"))]
#[inline(always)]
pub fn record_file(_bytes: u64) {}
```

- [ ] **Step 4: Register the module + `phase!` macro in `src/lib.rs`**

Add near the other `pub mod` lines:
```rust
pub mod metrics;
```
Add at the crate root (top level of `lib.rs`):
```rust
#[cfg(feature = "perf-metrics")]
#[macro_export]
macro_rules! phase {
    ($p:expr) => { $crate::metrics::Guard::new($p) };
}
#[cfg(not(feature = "perf-metrics"))]
#[macro_export]
macro_rules! phase {
    ($p:expr) => {{ () }};
}
```

- [ ] **Step 5: Verify both build configs compile**

```bash
cargo build --release
cargo build --release --features perf-metrics
```
Expected: both succeed. (The feature build currently pulls libc/walkdir/rand but uses none yet — that's fine.)

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/lib.rs src/metrics.rs
git commit -m "perf: add perf-metrics feature scaffold + Phase enum"
```

---

## Task 2: Metrics core (counters, Guard, getrusage, report)

**Files:** Modify `src/metrics.rs`; Test in `src/metrics.rs` (`#[cfg(test)]`).

- [ ] **Step 1: Add the feature-gated implementation module to `src/metrics.rs`**

Append:
```rust
#[cfg(feature = "perf-metrics")]
mod imp {
    use super::Phase;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    use std::time::{Duration, Instant};

    struct Stat { nanos: AtomicU64, calls: AtomicU64, bytes: AtomicU64 }
    impl Stat {
        const fn new() -> Self {
            Self { nanos: AtomicU64::new(0), calls: AtomicU64::new(0), bytes: AtomicU64::new(0) }
        }
    }
    static STATS: [Stat; Phase::COUNT] = [const { Stat::new() }; Phase::COUNT];
    static FILES: AtomicU64 = AtomicU64::new(0);
    static BYTES: AtomicU64 = AtomicU64::new(0);
    static SAMPLER_STOP: AtomicBool = AtomicBool::new(false);

    const NAMES: [&str; Phase::COUNT] = [
        "history_fetch", "history_parse", "bucket_stream", "xdr_decompress",
        "xdr_parse_ledger", "xdr_parse_tx", "xdr_parse_result", "xdr_parse_scp",
        "cross_file_verify", "chain_verify", "copy",
    ];

    pub struct Guard { idx: usize, start: Instant }
    impl Guard { pub fn new(p: Phase) -> Self { Self { idx: p as usize, start: Instant::now() } } }
    impl Drop for Guard {
        fn drop(&mut self) {
            let ns = self.start.elapsed().as_nanos() as u64;
            STATS[self.idx].nanos.fetch_add(ns, Relaxed);
            STATS[self.idx].calls.fetch_add(1, Relaxed);
        }
    }

    pub fn add_bytes(p: Phase, n: u64) { STATS[p as usize].bytes.fetch_add(n, Relaxed); }
    pub fn record_file(bytes: u64) { FILES.fetch_add(1, Relaxed); BYTES.fetch_add(bytes, Relaxed); }

    pub fn peak_rss_bytes() -> u64 {
        let mut ru = std::mem::MaybeUninit::<libc::rusage>::uninit();
        let max = unsafe {
            if libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) != 0 { return 0; }
            ru.assume_init().ru_maxrss as u64
        };
        if cfg!(target_os = "macos") { max } else { max * 1024 } // macOS bytes; Linux KiB
    }

    pub fn start_sampler() {
        let Ok(dir) = std::env::var("SA_PERF_OUT") else { return };
        let path = std::path::PathBuf::from(dir).join("timeseries.csv");
        let start = Instant::now();
        std::thread::spawn(move || {
            if let Some(p) = path.parent() { let _ = std::fs::create_dir_all(p); }
            let Ok(mut f) = std::fs::File::create(&path) else { return };
            let _ = writeln!(f, "t_s,files_done,bytes_done,peak_rss_mb");
            while !SAMPLER_STOP.load(Relaxed) {
                std::thread::sleep(Duration::from_secs(2));
                let _ = writeln!(f, "{:.1},{},{},{:.1}",
                    start.elapsed().as_secs_f64(), FILES.load(Relaxed), BYTES.load(Relaxed),
                    peak_rss_bytes() as f64 / 1e6);
                let _ = f.flush();
            }
        });
    }

    pub fn report(wall: Duration) {
        SAMPLER_STOP.store(true, Relaxed);
        let total_ns: u64 = (0..Phase::COUNT).map(|i| STATS[i].nanos.load(Relaxed)).sum();
        let peak_mb = peak_rss_bytes() as f64 / 1e6;
        let files = FILES.load(Relaxed);
        let bytes = BYTES.load(Relaxed);
        let mbps = if wall.as_secs_f64() > 0.0 { (bytes as f64 / 1e6) / wall.as_secs_f64() } else { 0.0 };
        eprintln!("PERF wall_ms={} peak_rss_mb={:.1} files={} bytes={} mb_per_s={:.1}",
            wall.as_millis(), peak_mb, files, bytes, mbps);
        for i in 0..Phase::COUNT {
            let ns = STATS[i].nanos.load(Relaxed);
            let pct = if total_ns > 0 { ns as f64 * 100.0 / total_ns as f64 } else { 0.0 };
            eprintln!("PERF_PHASE {},{:.1},{:.1},{},{:.1}", NAMES[i], ns as f64 / 1e6, pct,
                STATS[i].calls.load(Relaxed), STATS[i].bytes.load(Relaxed) as f64 / 1e6);
        }
        if let Ok(dir) = std::env::var("SA_PERF_OUT") {
            let dir = std::path::PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(mut f) = std::fs::File::create(dir.join("phases.csv")) {
                let _ = writeln!(f, "phase,total_ms,pct_of_phase_time,calls,mb,mb_per_s");
                for i in 0..Phase::COUNT {
                    let ns = STATS[i].nanos.load(Relaxed);
                    let pct = if total_ns > 0 { ns as f64 * 100.0 / total_ns as f64 } else { 0.0 };
                    let mb = STATS[i].bytes.load(Relaxed) as f64 / 1e6;
                    let secs = ns as f64 / 1e9;
                    let mbps2 = if secs > 0.0 { mb / secs } else { 0.0 };
                    let _ = writeln!(f, "{},{:.3},{:.2},{},{:.3},{:.2}", NAMES[i],
                        ns as f64 / 1e6, pct, STATS[i].calls.load(Relaxed), mb, mbps2);
                }
            }
            if let Ok(mut f) = std::fs::File::create(dir.join("headline.csv")) {
                let _ = writeln!(f, "wall_ms,peak_rss_mb,files,bytes,mb_per_s");
                let _ = writeln!(f, "{},{:.1},{},{},{:.2}", wall.as_millis(), peak_mb, files, bytes, mbps);
            }
        }
    }
}

#[cfg(feature = "perf-metrics")]
pub use imp::{add_bytes, peak_rss_bytes, record_file, report, start_sampler, Guard};
```

- [ ] **Step 2: Write a unit test (feature-gated)**

Append to `src/metrics.rs`:
```rust
#[cfg(all(test, feature = "perf-metrics"))]
mod tests {
    use super::*;
    #[test]
    fn guard_records_time_and_bytes() {
        { let _g = Guard::new(Phase::BucketStream); add_bytes(Phase::BucketStream, 1234); }
        // peak RSS should be a positive number on a running process
        assert!(peak_rss_bytes() > 0);
    }
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test --features perf-metrics --lib metrics::tests`
Expected: PASS.

- [ ] **Step 4: Verify the no-op build still compiles (no warnings about unused)**

Run: `cargo build --release`
Expected: success.

- [ ] **Step 5: Commit**

```bash
git add src/metrics.rs
git commit -m "perf: metrics core (atomic phase counters, getrusage RSS, CSV report)"
```

---

## Task 3: Wire wall timer + sampler + report into `main.rs`

**Files:** Modify `src/bin/stellar-archivist/main.rs`.

- [ ] **Step 1: Replace `main` body to time the run and emit metrics before exit**

```rust
use clap::Error;
use std::env;
use stellar_archivist::cli;

#[tokio::main]
async fn main() {
    #[cfg(feature = "perf-metrics")]
    let wall_start = std::time::Instant::now();
    #[cfg(feature = "perf-metrics")]
    stellar_archivist::metrics::start_sampler();

    let result = cli::run(env::args_os()).await;

    #[cfg(feature = "perf-metrics")]
    stellar_archivist::metrics::report(wall_start.elapsed());

    if let Err(e) = result {
        match e {
            cli::Error::Clap(e) => e.exit(),
            _ => Error::raw(clap::error::ErrorKind::ValueValidation, e).exit(),
        }
    }
}
```

- [ ] **Step 2: Verify a real instrumented run emits the headline (no phases wired yet → zeros)**

```bash
cargo build --release --features perf-metrics
SA_PERF_OUT=/tmp/perf-smoke target/release/stellar-archivist \
  scan file://$PWD/testdata/testnet-archive-small --high 1023 2>&1 | grep '^PERF '
cat /tmp/perf-smoke/headline.csv
```
Expected: a `PERF wall_ms=… peak_rss_mb=… …` line with nonzero wall/RSS; `headline.csv` exists.

- [ ] **Step 3: Commit**

```bash
git add src/bin/stellar-archivist/main.rs
git commit -m "perf: time the run and emit metrics report at exit (feature-gated)"
```

---

## Task 4: Insert phase guards into the hot paths

**Files:** Modify `src/verify.rs`, `src/xdr_verify.rs`, `src/pipeline.rs`, `src/storage.rs`.

For each insertion, add `let _g = crate::phase!(Phase::X);` as the **first statement** of the cited scope, with `use crate::metrics::Phase;` at the top of the file (or fully-qualify `crate::metrics::Phase::X`). Add `crate::metrics::add_bytes(Phase::X, n)` where bytes are known. These compile to nothing when the feature is off.

- [ ] **Step 1: `src/verify.rs` — `verify_bucket_internal` (~line 21)**

First statement of the function body:
```rust
let _g = crate::phase!(crate::metrics::Phase::BucketStream);
```
After the decompressed byte total is known (the hashed length), add:
```rust
crate::metrics::add_bytes(crate::metrics::Phase::BucketStream, total_decompressed_bytes);
```
(If the function doesn't track a running decompressed length, add a counter in the hash loop and pass it here. Keep it a single `u64`.)

- [ ] **Step 2: `src/xdr_verify.rs` — `decompress_and_write_internal` (~line 994)**

First statement:
```rust
let _g = crate::phase!(crate::metrics::Phase::XdrDecompress);
```
After decompression completes (decompressed `Vec` length known):
```rust
crate::metrics::add_bytes(crate::metrics::Phase::XdrDecompress, decompressed.len() as u64);
```

- [ ] **Step 3: `src/xdr_verify.rs` — the three parse loops + scp**

At the top of `parse_ledger_header_entries_for_checkpoint` (~621):
```rust
let _g = crate::phase!(crate::metrics::Phase::XdrParseLedger);
```
At the top of `parse_transaction_entries_for_checkpoint` (~876):
```rust
let _g = crate::phase!(crate::metrics::Phase::XdrParseTx);
```
At the top of `parse_result_entries_for_checkpoint` (~779):
```rust
let _g = crate::phase!(crate::metrics::Phase::XdrParseResult);
```
At the top of the SCP parse function (find `ScpHistoryEntry` `read_xdr_iter`):
```rust
let _g = crate::phase!(crate::metrics::Phase::XdrParseScp);
```

- [ ] **Step 4: `src/xdr_verify.rs` — verification functions**

Top of `verify_and_release` (~241):
```rust
let _g = crate::phase!(crate::metrics::Phase::CrossFileVerify);
```
Top of `verify_checkpoint_chain` (~536):
```rust
let _g = crate::phase!(crate::metrics::Phase::ChainVerify);
```

- [ ] **Step 5: `src/pipeline.rs` — history fetch/parse + file accounting**

Wrap the history download (`download_buffer` call in the history path) with a guard scope:
```rust
let buf = { let _g = crate::phase!(crate::metrics::Phase::HistoryFetch); /* existing download_buffer(...).await? */ };
crate::metrics::add_bytes(crate::metrics::Phase::HistoryFetch, buf.len() as u64);
```
Wrap the JSON parse + `validate()`:
```rust
let state = { let _g = crate::phase!(crate::metrics::Phase::HistoryParse); /* existing parse + validate */ };
```
Where the pipeline records a file as processed/success (the `record_success`/outcome site), add:
```rust
crate::metrics::record_file(0); // bytes counted in the phase that fetched/copied it
```

- [ ] **Step 6: `src/storage.rs` — `copy_from_reader` (~the fs copy path)**

First statement of `copy_from_reader`:
```rust
let _g = crate::phase!(crate::metrics::Phase::Copy);
```
After the byte count is known:
```rust
crate::metrics::add_bytes(crate::metrics::Phase::Copy, copied_bytes as u64);
```

- [ ] **Step 7: Verify production build is unaffected and instrumented build records phases**

```bash
cargo build --release                              # must compile, no new warnings
cargo build --release --features perf-metrics
SA_PERF_OUT=/tmp/perf-v target/release/stellar-archivist \
  scan file://$PWD/testdata/testnet-archive-small --verify 2>/dev/null
column -s, -t /tmp/perf-v/phases.csv
```
Expected: `phases.csv` shows **nonzero** `total_ms` for `history_fetch`, `history_parse`, `bucket_stream`, the `xdr_parse_*`, and `cross_file_verify` (scan --verify exercises all of these).

- [ ] **Step 8: Commit**

```bash
git add src/verify.rs src/xdr_verify.rs src/pipeline.rs src/storage.rs
git commit -m "perf: insert phase guards at hot-path boundaries"
```

---

## Task 5: Corruption module (`src/corruption.rs`)

**Files:** Create `src/corruption.rs`; Modify `src/lib.rs`.

- [ ] **Step 1: Register the module (feature-gated) in `src/lib.rs`**

```rust
#[cfg(feature = "corruption-tool")]
pub mod corruption;
```
(and gate the corruption test with `#[cfg(all(test, feature = "corruption-tool"))]`.)

- [ ] **Step 2: Create `src/corruption.rs` with ported helpers + the menu**

Port the four ledger helpers **verbatim** from `src/tests/utils.rs:498-577` (`read_and_parse_ledger_file`, `recompute_entry_hash`, `write_ledger_header_entries_to_file`, `corrupt_ledger_cross_file_hash`), changing `pub(crate)` → `pub` and removing the `#[cfg(test)]` context. Then add:

```rust
//! Archive corruption tool logic (feature-gated; perf testing only).
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde::Serialize;
use std::path::{Path, PathBuf};
use stellar_xdr::curr::Hash;
use walkdir::WalkDir;

// ── ported verbatim from src/tests/utils.rs:498-577 ───────────────────────────
// read_and_parse_ledger_file / recompute_entry_hash /
// write_ledger_header_entries_to_file / corrupt_ledger_cross_file_hash
// (pub, no #[cfg(test)])
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct Damage {
    pub kind: String,
    /// archive-relative object key, '/'-separated (matches the report format)
    pub key: String,
}

#[derive(Serialize, Default)]
pub struct Manifest { pub items: Vec<Damage> }

pub const ALL_KINDS: &[&str] = &[
    "delete-file", "truncate", "byte-flip", "invalid-gzip", "bucket-hash",
    "ledger-header-hash", "drop-ledger", "txset-hash", "result-hash",
    "intra-chain", "cross-chain", "well-known",
];

fn rel_key(abs: &Path, base: &Path) -> String {
    abs.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/")
}

fn files_matching(base: &Path, pat: &str) -> Vec<PathBuf> {
    WalkDir::new(base).into_iter().filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.to_string_lossy().replace('\\', "/").contains(pat))
        .collect()
}

fn pick<'a, R: Rng>(rng: &mut R, v: &'a [PathBuf]) -> Option<&'a PathBuf> { v.choose(rng) }

/// Apply one corruption of `kind` to `archive`, returning the Damage record.
pub fn apply(archive: &Path, kind: &str, rng: &mut impl Rng) -> Option<Damage> {
    let mk = |abs: &Path, k: &str| Damage { kind: k.to_string(), key: rel_key(abs, archive) };
    match kind {
        "delete-file" => {
            let files = files_matching(archive, "/").into_iter()
                .filter(|p| !p.to_string_lossy().contains(".well-known")).collect::<Vec<_>>();
            let p = pick(rng, &files)?.clone();
            let d = mk(&p, kind); std::fs::remove_file(&p).ok()?; Some(d)
        }
        "truncate" => { let files = files_matching(archive, ".xdr.gz");
            let p = pick(rng, &files)?.clone(); std::fs::write(&p, b"").ok()?; Some(mk(&p, kind)) }
        "byte-flip" => { let files = files_matching(archive, ".xdr.gz");
            let p = pick(rng, &files)?.clone();
            let mut b = std::fs::read(&p).ok()?; for x in &mut b { *x ^= 0xff; }
            std::fs::write(&p, b).ok()?; Some(mk(&p, kind)) }
        "invalid-gzip" => { let files = files_matching(archive, "/bucket-");
            let p = pick(rng, &files)?.clone();
            std::fs::write(&p, b"not gzip at all").ok()?; Some(mk(&p, kind)) }
        "bucket-hash" => { let files = files_matching(archive, "/bucket-");
            let p = pick(rng, &files)?.clone();
            // valid gzip, wrong content
            use flate2::{write::GzEncoder, Compression};
            use std::io::Write as _;
            let mut e = GzEncoder::new(Vec::new(), Compression::default());
            e.write_all(b"valid gzip wrong content").ok()?;
            std::fs::write(&p, e.finish().ok()?).ok()?; Some(mk(&p, kind)) }
        "ledger-header-hash" => { let files = files_matching(archive, "/ledger-");
            let p = pick(rng, &files)?.clone();
            let mut es = read_and_parse_ledger_file(&p);
            if let Some(e) = es.get_mut(0) { e.hash = Hash([0xDE; 32]); } // hash no longer matches header
            write_ledger_header_entries_to_file(&p, &es); Some(mk(&p, kind)) }
        "drop-ledger" => { let files = files_matching(archive, "/ledger-");
            let p = pick(rng, &files)?.clone();
            let mut es = read_and_parse_ledger_file(&p);
            if es.len() > 1 { es.remove(es.len() / 2); }
            write_ledger_header_entries_to_file(&p, &es); Some(mk(&p, kind)) }
        "txset-hash" => { let files = files_matching(archive, "/ledger-");
            let p = pick(rng, &files)?.clone();
            corrupt_ledger_cross_file_hash(&p, "tx_set"); Some(mk(&p, kind)) }
        "result-hash" => { let files = files_matching(archive, "/ledger-");
            let p = pick(rng, &files)?.clone();
            corrupt_ledger_cross_file_hash(&p, "result"); Some(mk(&p, kind)) }
        "intra-chain" => { let files = files_matching(archive, "/ledger-");
            let p = pick(rng, &files)?.clone();
            let mut es = read_and_parse_ledger_file(&p);
            if es.len() > 1 { es[1].header.previous_ledger_hash = Hash([0xAB; 32]); recompute_entry_hash(&mut es[1]); }
            write_ledger_header_entries_to_file(&p, &es); Some(mk(&p, kind)) }
        "cross-chain" => { let files = files_matching(archive, "/ledger-");
            let p = pick(rng, &files)?.clone();
            let mut es = read_and_parse_ledger_file(&p);
            if let Some(e) = es.get_mut(0) { e.header.previous_ledger_hash = Hash([0xCD; 32]); recompute_entry_hash(e); }
            write_ledger_header_entries_to_file(&p, &es); Some(mk(&p, kind)) }
        "well-known" => { let p = archive.join(".well-known/stellar-history.json");
            std::fs::write(&p, b"{ invalid json }}}").ok()?; Some(mk(&p, kind)) }
        _ => None,
    }
}

pub fn run(archive: &Path, kinds: &[String], count: usize, seed: u64) -> Manifest {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut m = Manifest::default();
    for _ in 0..count {
        let k = kinds.choose(&mut rng).cloned().unwrap_or_default();
        if let Some(d) = apply(archive, &k, &mut rng) { m.items.push(d); }
    }
    m
}
```

- [ ] **Step 3: Write an integration test — each kind is detected by `scan --verify`**

Create `src/tests/corruption_test.rs` (gated) with a test that, for each kind: copies `testnet-archive-small` to a temp dir, applies that one corruption, runs an in-process scan with `--verify`, and asserts the scan reports a failure. Register `#[cfg(all(test, feature = "perf-metrics"))] mod corruption_test;` in `src/tests/mod.rs`.

```rust
#[cfg(all(test, feature = "perf-metrics"))]
#[tokio::test]
async fn each_corruption_is_detected() {
    use crate::corruption;
    for kind in corruption::ALL_KINDS {
        let dir = tempfile::TempDir::new().unwrap();
        crate::tests::utils::copy_testnet_small_archive(dir.path()).unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        corruption::apply(dir.path(), kind, &mut rng).expect(kind);
        let cfg = crate::test_helpers::ScanConfig::new(crate::tests::utils::file_url_from_path(dir.path()))
            .verify();
        let res = crate::test_helpers::run_scan(cfg).await;
        assert!(res.is_err(), "scan --verify should detect corruption kind `{kind}`");
    }
}
```

- [ ] **Step 4: Run the test**

Run: `cargo test --features perf-metrics --lib corruption_test`
Expected: PASS (every kind detected). If a kind isn't detected, fix that arm before proceeding.

- [ ] **Step 5: Commit**

```bash
git add src/corruption.rs src/lib.rs src/tests/mod.rs src/tests/corruption_test.rs
git commit -m "perf: corruption module (ported helpers + full menu) with detection test"
```

---

## Task 6: `corrupt-archive` binary

**Files:** Create `src/bin/corrupt-archive/main.rs`; Modify `Cargo.toml`.

- [ ] **Step 1: Add the bin to `Cargo.toml`** (after the existing `[[bin]]`)

```toml
[[bin]]
name = "corrupt-archive"
path = "src/bin/corrupt-archive/main.rs"
required-features = ["cli", "corruption-tool"]
doctest = false
```

- [ ] **Step 2: Create `src/bin/corrupt-archive/main.rs`**

```rust
use clap::Parser;
use std::path::PathBuf;
use stellar_archivist::corruption;

#[derive(Parser)]
#[command(about = "Corrupt a local archive for repair testing (perf-metrics only)")]
struct Args {
    /// Local archive directory to corrupt in place
    archive: PathBuf,
    /// Comma-separated corruption kinds, or "all"
    #[arg(long, default_value = "all")]
    kinds: String,
    #[arg(long, default_value_t = 10)]
    count: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Write a JSON manifest of what was broken
    #[arg(long)]
    manifest: Option<PathBuf>,
}

fn main() {
    let a = Args::parse();
    let kinds: Vec<String> = if a.kinds == "all" {
        corruption::ALL_KINDS.iter().map(|s| s.to_string()).collect()
    } else {
        a.kinds.split(',').map(|s| s.trim().to_string()).collect()
    };
    let m = corruption::run(&a.archive, &kinds, a.count, a.seed);
    eprintln!("corrupt-archive: applied {} corruption(s)", m.items.len());
    if let Some(path) = a.manifest {
        std::fs::write(&path, serde_json::to_string_pretty(&m).unwrap()).expect("write manifest");
    }
}
```

- [ ] **Step 3: Build + smoke test the full corrupt→detect→repair loop**

```bash
cargo build --release --features perf-metrics
cp target/release/stellar-archivist bin/sa-perf 2>/dev/null || { mkdir -p bin && cp target/release/stellar-archivist bin/sa-perf; }
cp target/release/corrupt-archive bin/corrupt-archive
W=$(mktemp -d); cp -r testdata/testnet-archive-small "$W/a"; cp -r "$W/a" "$W/snap"
bin/corrupt-archive "$W/a" --kinds all --count 15 --seed 3 --manifest "$W/m.json"
bin/sa-perf scan "file://$W/a" --verify --report "$W/scan.json" || echo "detected (expected non-zero)"
bin/sa-perf repair "file://$PWD/testdata/testnet-archive-small" "file://$W/a" --verify --report "$W/repair.json"
diff -r "$W/snap" "$W/a" && echo "RESTORED-IDENTICAL" || echo "DIFF (check .well-known rule)"
```
Expected: corruption applied; scan reports failures; repair exits 0; `diff -r` empty (or only `.well-known`, satisfying §5.2's rule).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml src/bin/corrupt-archive/main.rs
git commit -m "perf: corrupt-archive binary + manifest"
```

---

## Task 7: Harness scripts

**Files:** Create `scripts/perf/env.sh`, `run.sh`, `scaling.sh`, `plot.py`; Modify `.gitignore`.

- [ ] **Step 1: `.gitignore`** — append:
```
/perf-results/
/bin/
```

- [ ] **Step 2: `scripts/perf/env.sh`** (capture environment)

```bash
#!/usr/bin/env bash
set -euo pipefail
OUT=${1:-perf-results}; mkdir -p "$OUT"
{
  echo "date: $(date -u +%FT%TZ)"; echo "host: $(hostname)"; echo "os: $(uname -a)"
  if [[ "$(uname)" == "Darwin" ]]; then
    echo "cpu: $(sysctl -n machdep.cpu.brand_string)"; echo "cores_logical: $(sysctl -n hw.logicalcpu)"
    echo "cores_physical: $(sysctl -n hw.physicalcpu)"; echo "ram_bytes: $(sysctl -n hw.memsize)"
  else
    echo "cpu: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)"
    echo "cores_logical: $(nproc)"; echo "ram_kb: $(grep MemTotal /proc/meminfo | awk '{print $2}')"
  fi
  echo "rustc: $(rustc --version)"
} | tee "$OUT/env.txt"
```

- [ ] **Step 3: `scripts/perf/run.sh`** (one measured run: OS time + ps sampler + run.csv row)

```bash
#!/usr/bin/env bash
# Usage: run.sh <run-id> <binary> <op> <args...>
set -uo pipefail
RID="$1"; BIN="$2"; shift 2
ROOT="perf-results/$RID"; mkdir -p "$ROOT"
export SA_PERF_OUT="$ROOT"
echo "$BIN $*" > "$ROOT/cmd.txt"

# OS time wrapper differs per platform
if [[ "$(uname)" == "Darwin" ]]; then TIMER=(/usr/bin/time -l); RSS_DIV=1048576       # bytes -> MB
else TIMER=(/usr/bin/time -v); RSS_DIV=1024; fi                                       # KB -> MB

# background RSS sampler via ps (KB)
( echo "t_s,rss_kb" > "$ROOT/ps_rss.csv"; S=$(date +%s)
  while true; do P=$(pgrep -n -f "$BIN $1" || true); [[ -z "$P" ]] && break
    echo "$(( $(date +%s) - S )),$(ps -o rss= -p "$P" 2>/dev/null | tr -d ' ')" >> "$ROOT/ps_rss.csv"; sleep 2; done ) &
SAMPLER=$!

"${TIMER[@]}" "$BIN" "$@" > "$ROOT/stdout.log" 2> "$ROOT/time_stderr.log"
EXIT=$?
kill "$SAMPLER" 2>/dev/null || true

# parse peak RSS + wall from the OS timer output
if [[ "$(uname)" == "Darwin" ]]; then
  PEAK=$(grep 'maximum resident set size' "$ROOT/time_stderr.log" | awk '{print $1}')
  WALL=$(grep -E 'real' "$ROOT/time_stderr.log" | awk '{print $1}')   # may be in 'real' line; else use PERF line
else
  PEAK=$(grep 'Maximum resident set size' "$ROOT/time_stderr.log" | awk -F': ' '{print $2}')
  WALL=$(grep 'Elapsed (wall clock)' "$ROOT/time_stderr.log" | awk -F': ' '{print $2}')
fi
PEAK_MB=$(awk -v p="${PEAK:-0}" -v d="$RSS_DIV" 'BEGIN{printf "%.1f", p/d}')
# headline from in-process metrics (authoritative wall_ms if present)
PERF=$(grep '^PERF ' "$ROOT/time_stderr.log" || true)
echo "run_id,exit,peak_rss_mb_os,perf_line" > "$ROOT/run.csv"
echo "$RID,$EXIT,$PEAK_MB,\"$PERF\"" >> "$ROOT/run.csv"
echo "[$RID] exit=$EXIT peak_rss_mb(os)=$PEAK_MB  $PERF"
```

- [ ] **Step 4: `scripts/perf/scaling.sh`** (sweep `-C` × modes × reps → summary.csv)

```bash
#!/usr/bin/env bash
# Usage: scaling.sh <src-url> <bin>   (bin = bin/sa-clean or bin/sa-perf)
set -uo pipefail
SRC="$1"; BIN="${2:-bin/sa-clean}"
CS=(1 2 4 8 16 32 64); REPS=3
echo "run_id,mode,concurrency,rep" > perf-results/summary_index.csv
for C in "${CS[@]}"; do for R in $(seq 1 $REPS); do
  scripts/perf/run.sh "scan_C${C}_r${R}"        "$BIN" scan   "$SRC" -C "$C"
  scripts/perf/run.sh "scanverify_C${C}_r${R}"  "$BIN" scan   "$SRC" -C "$C" --verify
  D=$(mktemp -d); scripts/perf/run.sh "mirror_C${C}_r${R}" "$BIN" mirror "$SRC" "file://$D" -C "$C"; rm -rf "$D"
done; done
echo "Done. Aggregate perf-results/*/headline.csv + run.csv, then: scripts/perf/plot.py"
```

- [ ] **Step 5: `scripts/perf/plot.py`** (CSV → PNGs)

```python
#!/usr/bin/env python3
"""Plot perf results. Usage: plot.py <summary.csv>  (columns: mode,concurrency,wall_ms,peak_rss_mb)"""
import sys, csv, collections
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

rows = list(csv.DictReader(open(sys.argv[1])))
by_mode = collections.defaultdict(list)
for r in rows:
    by_mode[r["mode"]].append((int(r["concurrency"]), float(r["wall_ms"]), float(r["peak_rss_mb"])))
for metric, idx, ylabel, fname in [("wall", 1, "wall time (ms)", "time_vs_concurrency.png"),
                                   ("rss", 2, "peak RSS (MB)", "rss_vs_concurrency.png")]:
    plt.figure()
    for mode, pts in by_mode.items():
        pts = sorted(pts)
        plt.plot([p[0] for p in pts], [p[idx] for p in pts], marker="o", label=mode)
    plt.xlabel("concurrency (-C)"); plt.ylabel(ylabel); plt.legend(); plt.grid(True)
    plt.savefig(f"perf-results/plots/{fname}", dpi=120, bbox_inches="tight")
print("wrote perf-results/plots/*.png")
```

- [ ] **Step 6: Make executable + smoke test on the small archive**

```bash
chmod +x scripts/perf/*.sh scripts/perf/plot.py
mkdir -p perf-results/plots
scripts/perf/env.sh
scripts/perf/run.sh smoke bin/sa-clean scan "file://$PWD/testdata/testnet-archive-small" --high 1023
cat perf-results/smoke/run.csv
```
Expected: `env.txt` populated; `perf-results/smoke/run.csv` has a row with `exit=0` and a peak RSS value.

- [ ] **Step 7: Commit**

```bash
git add scripts/perf .gitignore
git commit -m "perf: harness scripts (env, run, scaling sweep, plot)"
```

---

## Task 8: Wire run.csv aggregation + final self-check

**Files:** Modify `scripts/perf/run.sh` (merge in-process headline into run.csv); add `scripts/perf/README.md`.

- [ ] **Step 1: Have `run.sh` also copy the in-process `headline.csv`/`phases.csv`** (already written to `$SA_PERF_OUT=$ROOT` by the instrumented binary) and assemble a normalized `summary.csv` row when `headline.csv` exists. Add at the end of `run.sh`:

```bash
if [[ -f "$ROOT/headline.csv" ]]; then
  HL=$(tail -1 "$ROOT/headline.csv")   # wall_ms,peak_rss_mb,files,bytes,mb_per_s
  echo "$RID,$HL,$PEAK_MB" >> perf-results/summary.csv
fi
```
And create the header once in `scaling.sh` before the loop:
```bash
echo "run_id,wall_ms,peak_rss_mb,files,bytes,mb_per_s,peak_rss_mb_os" > perf-results/summary.csv
```
(Use `bin/sa-perf` as the bin in `scaling.sh` so `headline.csv` is produced; or run each config once with `sa-perf` for phases + headline and the reps with `sa-clean` for clean wall numbers.)

- [ ] **Step 2: `scripts/perf/README.md`** — 10-line how-to pointing at `docs/perf-testing-plan.md` and listing the script entry points.

- [ ] **Step 3: Final self-check — both binaries build; clean build has zero perf overhead surface**

```bash
cargo build --release && echo "clean OK"
cargo build --release --features perf-metrics && echo "perf OK"
cargo test --features perf-metrics --lib metrics corruption_test
# Confirm production binary has no perf symbols:
nm target/release/stellar-archivist 2>/dev/null | grep -i 'metrics::imp' && echo "LEAK" || echo "clean (no perf symbols)"
```
Expected: both build; tests pass; "clean (no perf symbols)".

- [ ] **Step 4: Commit**

```bash
git add scripts/perf/run.sh scripts/perf/scaling.sh scripts/perf/README.md
git commit -m "perf: aggregate summary.csv + harness readme"
```

---

## Self-review notes (planner)

- **Spec coverage:** §2 instrumentation → Tasks 2–4; §3 corruption → Tasks 5–6; §6.0 OS-time/ps methodology → Task 7 `run.sh`; §6.1 scaling+plots → Task 7 `scaling.sh`/`plot.py`; §0 feature-gated/off-PR → Task 1 + `.gitignore`; §2.5 sa-clean/sa-perf → Tasks 3/6/8. Stage 1 correctness is executed via the existing binary + `corrupt-archive` (no new code beyond Task 6's loop).
- **Zero-cost-off:** verified by Task 8 Step 3 (`nm` check) + the `cargo build --release` checks throughout.
- **Known executor judgment points (not placeholders):** the exact byte-counter variable in `verify_bucket_internal` (Task 4 Step 1) and the SCP parse function name (Step 3) — both are located by the cited anchors; the executor wires the single `u64` through.
