//! Lightweight, feature-gated performance instrumentation.
//!
//! Compiled to no-ops unless built with `--features perf-metrics`. Phase timers
//! aggregate across concurrent tasks via atomics. See `docs/perf-testing-plan.md` §2.
//! Phase times overlap (concurrency + spawned tasks) and form a profiler-style
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
}

/// Always-on diagnostic gauge: number of decode tasks (gzip+SHA bucket `hash_task`
/// and gzip xdr `decompress_task`) currently *running*. Lets us see, in-runtime, how
/// many decodes are concurrently alive — distinguishing "orchestration only creates a
/// few" from "many exist but sit starved/unscheduled". Cheap atomic; not gated.
pub static ACTIVE_DECODES: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// RAII guard for [`ACTIVE_DECODES`]: `enter()` at decode-task start, decrement on drop.
pub struct DecodeGuard;
impl DecodeGuard {
    pub fn enter() -> Self {
        ACTIVE_DECODES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        DecodeGuard
    }
}
impl Drop for DecodeGuard {
    fn drop(&mut self) {
        ACTIVE_DECODES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(feature = "perf-metrics")]
const NAMES: [&str; Phase::COUNT] = [
    "history_fetch",
    "history_parse",
    "bucket_stream",
    "xdr_decompress",
    "xdr_parse_ledger",
    "xdr_parse_tx",
    "xdr_parse_result",
    "xdr_parse_scp",
    "cross_file_verify",
    "chain_verify",
    "copy",
];

#[cfg(feature = "perf-metrics")]
mod imp {
    use super::{Phase, NAMES};
    use std::io::Write as _;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    use std::time::{Duration, Instant};

    struct Stat {
        nanos: AtomicU64,     // wall-clock self-time (Instant), summed over concurrent calls
        cpu_nanos: AtomicU64, // thread CPU self-time (CLOCK_THREAD_CPUTIME_ID); only recorded
        // for sync-scoped phases (see SYNC) where the scope starts/ends on one thread.
        calls: AtomicU64,
        bytes: AtomicU64,
    }
    impl Stat {
        const fn new() -> Self {
            Self {
                nanos: AtomicU64::new(0),
                cpu_nanos: AtomicU64::new(0),
                calls: AtomicU64::new(0),
                bytes: AtomicU64::new(0),
            }
        }
    }

    /// Per-phase scope discipline. `true` = the phase scope is **synchronous** (no `.await`
    /// inside, starts and ends on the same thread) so a `CLOCK_THREAD_CPUTIME_ID` delta is a
    /// valid, **preemption-immune** CPU measurement. `false` = the scope spans `.await`
    /// points (I/O, channel feed, `JoinHandle.await`) and may resume on another worker, so a
    /// per-thread CPU delta is meaningless — we record wall only and leave cpu_nanos = 0.
    /// Order matches `Phase`: HistoryFetch, HistoryParse, BucketStream, XdrDecompress,
    /// XdrParseLedger, XdrParseTx, XdrParseResult, XdrParseScp, CrossFileVerify, ChainVerify, Copy.
    const SYNC: [bool; Phase::COUNT] = [
        false, // HistoryFetch  — network/IO + await
        true,  // HistoryParse  — synchronous parse
        false, // BucketStream  — into_stream + feed loop + hash_task.await (or bulk read + spawn_blocking.await)
        false, // XdrDecompress — feed + decompress_task.await
        true,  // XdrParseLedger
        true,  // XdrParseTx
        true,  // XdrParseResult
        true,  // XdrParseScp
        true,  // CrossFileVerify
        true,  // ChainVerify
        true,  // Copy
    ];

    /// Per-thread CPU time in nanoseconds (excludes time the thread was de-scheduled), via
    /// `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`. vDSO call (~tens of ns); safe in hot paths.
    fn thread_cpu_nanos() -> u64 {
        let mut ts = std::mem::MaybeUninit::<libc::timespec>::uninit();
        unsafe {
            if libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, ts.as_mut_ptr()) != 0 {
                return 0;
            }
            let ts = ts.assume_init();
            (ts.tv_sec as u64)
                .wrapping_mul(1_000_000_000)
                .wrapping_add(ts.tv_nsec as u64)
        }
    }

    /// Total process CPU time (user + system) in nanoseconds, across all threads, via
    /// `getrusage(RUSAGE_SELF)`. Lets us recover decode CPU by subtraction even though the
    /// async decode scopes aren't CPU-timed: decode ≈ total − Σ(sync-phase CPU).
    fn process_cpu_nanos() -> u64 {
        let mut ru = std::mem::MaybeUninit::<libc::rusage>::uninit();
        unsafe {
            if libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) != 0 {
                return 0;
            }
            let ru = ru.assume_init();
            let to_ns = |tv: libc::timeval| {
                (tv.tv_sec as u64)
                    .wrapping_mul(1_000_000_000)
                    .wrapping_add((tv.tv_usec as u64).wrapping_mul(1000))
            };
            to_ns(ru.ru_utime).wrapping_add(to_ns(ru.ru_stime))
        }
    }

    static STATS: [Stat; Phase::COUNT] = [const { Stat::new() }; Phase::COUNT];
    static FILES: AtomicU64 = AtomicU64::new(0);
    static BYTES: AtomicU64 = AtomicU64::new(0);
    static SAMPLER_STOP: AtomicBool = AtomicBool::new(false);

    /// RAII timer: records wall-clock self-time (always) and thread-CPU self-time (for
    /// sync-scoped phases only — see [`SYNC`]) plus a call count, on drop.
    pub struct Guard {
        idx: usize,
        start: Instant,
        cpu_start: u64,
    }
    impl Guard {
        pub fn new(p: Phase) -> Self {
            let idx = p as usize;
            // Only pay for the CPU clock on phases where it's meaningful.
            let cpu_start = if SYNC[idx] { thread_cpu_nanos() } else { 0 };
            Self {
                idx,
                start: Instant::now(),
                cpu_start,
            }
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let ns = self.start.elapsed().as_nanos() as u64;
            STATS[self.idx].nanos.fetch_add(ns, Relaxed);
            STATS[self.idx].calls.fetch_add(1, Relaxed);
            if SYNC[self.idx] {
                let cpu_end = thread_cpu_nanos();
                // Guard against a clock that went backwards (e.g. the rare case the scope
                // still got moved across threads): only credit a non-negative delta.
                if cpu_end >= self.cpu_start {
                    STATS[self.idx]
                        .cpu_nanos
                        .fetch_add(cpu_end - self.cpu_start, Relaxed);
                }
            }
        }
    }

    pub fn add_bytes(p: Phase, n: u64) {
        STATS[p as usize].bytes.fetch_add(n, Relaxed);
    }
    pub fn record_file(bytes: u64) {
        FILES.fetch_add(1, Relaxed);
        BYTES.fetch_add(bytes, Relaxed);
    }

    /// Peak resident set size in **bytes**, normalized across platforms.
    /// `getrusage(RUSAGE_SELF).ru_maxrss` is bytes on macOS and kilobytes on Linux.
    pub fn peak_rss_bytes() -> u64 {
        let mut ru = std::mem::MaybeUninit::<libc::rusage>::uninit();
        let max = unsafe {
            if libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) != 0 {
                return 0;
            }
            ru.assume_init().ru_maxrss as u64
        };
        #[cfg(target_os = "macos")]
        {
            max // already bytes
        }
        #[cfg(not(target_os = "macos"))]
        {
            max.saturating_mul(1024) // Linux & other POSIX report kibibytes
        }
    }

    pub fn start_sampler() {
        let Ok(dir) = std::env::var("SA_PERF_OUT") else {
            return;
        };
        let path = std::path::PathBuf::from(dir).join("timeseries.csv");
        let start = Instant::now();
        std::thread::spawn(move || {
            if let Some(p) = path.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let Ok(mut f) = std::fs::File::create(&path) else {
                return;
            };
            let _ = writeln!(f, "t_s,files_done,bytes_done,peak_rss_mb");
            while !SAMPLER_STOP.load(Relaxed) {
                std::thread::sleep(Duration::from_secs(2));
                let _ = writeln!(
                    f,
                    "{:.1},{},{},{:.1}",
                    start.elapsed().as_secs_f64(),
                    FILES.load(Relaxed),
                    BYTES.load(Relaxed),
                    peak_rss_bytes() as f64 / 1e6
                );
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
        let mbps = if wall.as_secs_f64() > 0.0 {
            (bytes as f64 / 1e6) / wall.as_secs_f64()
        } else {
            0.0
        };
        eprintln!(
            "PERF wall_ms={} peak_rss_mb={:.1} files={} bytes={} mb_per_s={:.1}",
            wall.as_millis(),
            peak_mb,
            files,
            bytes,
            mbps
        );
        // Total process CPU (all threads) and the share captured by sync-scoped phases.
        // decode CPU (not directly timed — its scope is async) ≈ total − sync_cpu − history.
        let total_cpu_ns = process_cpu_nanos();
        let sync_cpu_ns: u64 = (0..Phase::COUNT)
            .map(|i| STATS[i].cpu_nanos.load(Relaxed))
            .sum();
        eprintln!(
            "PERF_CPU total_cpu_ms={:.1} sync_phase_cpu_ms={:.1} other_cpu_ms={:.1} cores_avg={:.2}",
            total_cpu_ns as f64 / 1e6,
            sync_cpu_ns as f64 / 1e6,
            total_cpu_ns.saturating_sub(sync_cpu_ns) as f64 / 1e6,
            if wall.as_secs_f64() > 0.0 {
                total_cpu_ns as f64 / 1e9 / wall.as_secs_f64()
            } else {
                0.0
            }
        );
        // Columns: name, wall_self_ms, wall_pct, calls, mb, cpu_self_ms, scope.
        // NOTE: wall_self includes .await wait for async-scoped phases — it is NOT a CPU
        // measure for those. Use cpu_self (sync phases only) for CPU; decode CPU is the
        // PERF_CPU `other_cpu_ms` residual. Do not compare wall_pct across phase scopes.
        for i in 0..Phase::COUNT {
            let ns = STATS[i].nanos.load(Relaxed);
            let pct = if total_ns > 0 {
                ns as f64 * 100.0 / total_ns as f64
            } else {
                0.0
            };
            eprintln!(
                "PERF_PHASE {},{:.1},{:.1},{},{:.1},{:.1},{}",
                NAMES[i],
                ns as f64 / 1e6,
                pct,
                STATS[i].calls.load(Relaxed),
                STATS[i].bytes.load(Relaxed) as f64 / 1e6,
                STATS[i].cpu_nanos.load(Relaxed) as f64 / 1e6,
                if SYNC[i] { "cpu" } else { "wall-incl-wait" }
            );
        }
        if let Ok(dir) = std::env::var("SA_PERF_OUT") {
            let dir = std::path::PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(mut f) = std::fs::File::create(dir.join("phases.csv")) {
                let _ = writeln!(
                    f,
                    "phase,wall_ms,wall_pct,calls,mb,mb_per_s,cpu_ms,scope"
                );
                for i in 0..Phase::COUNT {
                    let ns = STATS[i].nanos.load(Relaxed);
                    let pct = if total_ns > 0 {
                        ns as f64 * 100.0 / total_ns as f64
                    } else {
                        0.0
                    };
                    let mb = STATS[i].bytes.load(Relaxed) as f64 / 1e6;
                    let secs = ns as f64 / 1e9;
                    let phase_mbps = if secs > 0.0 { mb / secs } else { 0.0 };
                    let _ = writeln!(
                        f,
                        "{},{:.3},{:.2},{},{:.3},{:.2},{:.3},{}",
                        NAMES[i],
                        ns as f64 / 1e6,
                        pct,
                        STATS[i].calls.load(Relaxed),
                        mb,
                        phase_mbps,
                        STATS[i].cpu_nanos.load(Relaxed) as f64 / 1e6,
                        if SYNC[i] { "cpu" } else { "wall-incl-wait" }
                    );
                }
                // Total process CPU (getrusage) for decode-by-subtraction analysis.
                let total_cpu_ns = process_cpu_nanos();
                let sync_cpu_ns: u64 = (0..Phase::COUNT)
                    .map(|i| STATS[i].cpu_nanos.load(Relaxed))
                    .sum();
                let _ = writeln!(
                    f,
                    "_total_process,{:.3},,,,,{:.3},cpu",
                    wall.as_millis() as f64,
                    total_cpu_ns as f64 / 1e6
                );
                let _ = writeln!(
                    f,
                    "_decode_residual,,,,,,{:.3},cpu",
                    total_cpu_ns.saturating_sub(sync_cpu_ns) as f64 / 1e6
                );
            }
            if let Ok(mut f) = std::fs::File::create(dir.join("headline.csv")) {
                let _ = writeln!(f, "wall_ms,peak_rss_mb,files,bytes,mb_per_s");
                let _ = writeln!(
                    f,
                    "{},{:.1},{},{},{:.2}",
                    wall.as_millis(),
                    peak_mb,
                    files,
                    bytes,
                    mbps
                );
            }
        }
    }
}

#[cfg(feature = "perf-metrics")]
pub use imp::{add_bytes, peak_rss_bytes, record_file, report, start_sampler, Guard};

// ── No-op surface when the feature is off (everything inlines away) ───────────
#[cfg(not(feature = "perf-metrics"))]
#[inline(always)]
pub fn add_bytes(_p: Phase, _n: u64) {}
#[cfg(not(feature = "perf-metrics"))]
#[inline(always)]
pub fn record_file(_bytes: u64) {}

#[cfg(all(test, feature = "perf-metrics"))]
mod tests {
    use super::*;

    #[test]
    fn guard_records_and_rss_is_sane() {
        {
            let _g = Guard::new(Phase::BucketStream);
            add_bytes(Phase::BucketStream, 4096);
            record_file(4096);
        }
        // A live process must have a positive peak RSS, and it should be a
        // plausible magnitude (between 1 MB and 100 GB) — catches unit mistakes.
        let rss = peak_rss_bytes();
        assert!(rss > 1_000_000, "peak RSS too small ({rss} bytes) — unit bug?");
        assert!(rss < 100_000_000_000, "peak RSS implausibly large ({rss} bytes) — unit bug?");
        eprintln!("peak_rss_bytes() = {rss} ({:.1} MB)", rss as f64 / 1e6);
    }
}
