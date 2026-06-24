//! Lightweight, feature-gated performance instrumentation.
//!
//! Compiled to no-ops unless built with `--features perf-metrics`. Phase timers
//! aggregate elapsed time across concurrent tasks via atomics. Phase times can
//! overlap, so percentages are a profiler-style breakdown rather than a
//! wall-clock critical path.

#[repr(usize)]
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

/// Heartbeat probe tick. A spawned task sleeps this long each beat and records
/// how much later than this it actually woke (scheduler/timer lag); a growing
/// tail means the worker pool is too busy to promptly poll a ready task.
pub const HEARTBEAT_TICK_MS: u64 = 10;

/// Diagnostic gauge for Tokio runtime metrics: number of decode tasks (gzip+SHA
/// bucket `hash_task` and gzip XDR `decompress_task`) currently alive.
#[cfg(all(feature = "perf-metrics", tokio_unstable))]
pub static ACTIVE_DECODES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// RAII guard for [`ACTIVE_DECODES`]: `enter()` at decode-task start, decrement on drop.
pub struct DecodeGuard;
impl DecodeGuard {
    #[inline(always)]
    pub fn enter() -> Self {
        #[cfg(all(feature = "perf-metrics", tokio_unstable))]
        ACTIVE_DECODES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        DecodeGuard
    }
}
impl Drop for DecodeGuard {
    fn drop(&mut self) {
        #[cfg(all(feature = "perf-metrics", tokio_unstable))]
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
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::time::{Duration, Instant};

    struct Stat {
        nanos: AtomicU64, // elapsed wall time (Instant), summed over concurrent calls
        calls: AtomicU64,
        bytes: AtomicU64,
    }
    impl Stat {
        const fn new() -> Self {
            Self {
                nanos: AtomicU64::new(0),
                calls: AtomicU64::new(0),
                bytes: AtomicU64::new(0),
            }
        }
    }

    /// Total process CPU time (user + system) in nanoseconds, across all threads.
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
    // Completed file-level observations. BYTES is phase-specific: e.g. copy
    // phases count compressed bytes, while verify phases count decompressed bytes.
    static FILES: AtomicU64 = AtomicU64::new(0);
    static BYTES: AtomicU64 = AtomicU64::new(0);

    /// RAII timer: records elapsed wall time plus a call count, on drop.
    pub struct Guard {
        idx: usize,
        start: Instant,
    }
    impl Guard {
        pub fn new(p: Phase) -> Self {
            Self {
                idx: p as usize,
                start: Instant::now(),
            }
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let ns = self.start.elapsed().as_nanos() as u64;
            STATS[self.idx].nanos.fetch_add(ns, Relaxed);
            STATS[self.idx].calls.fetch_add(1, Relaxed);
        }
    }

    pub fn add_bytes(p: Phase, n: u64) {
        STATS[p as usize].bytes.fetch_add(n, Relaxed);
    }
    pub fn record_file(bytes: u64) {
        FILES.fetch_add(1, Relaxed);
        BYTES.fetch_add(bytes, Relaxed);
    }

    // Heartbeat (runtime-responsiveness) accumulators. lag = how much later than
    // HEARTBEAT_TICK_MS a beat actually woke; the over_*ms tails are the signal
    // that the worker pool is too saturated to promptly poll a ready task.
    static HB_BEATS: AtomicU64 = AtomicU64::new(0);
    static HB_SUM_NS: AtomicU64 = AtomicU64::new(0);
    static HB_MAX_NS: AtomicU64 = AtomicU64::new(0);
    static HB_OVER_1MS: AtomicU64 = AtomicU64::new(0);
    static HB_OVER_10MS: AtomicU64 = AtomicU64::new(0);
    static HB_OVER_100MS: AtomicU64 = AtomicU64::new(0);

    /// Record one heartbeat's scheduling lag (actual wake delay beyond the tick).
    pub fn record_heartbeat(lag: Duration) {
        let ns = lag.as_nanos() as u64;
        HB_BEATS.fetch_add(1, Relaxed);
        HB_SUM_NS.fetch_add(ns, Relaxed);
        HB_MAX_NS.fetch_max(ns, Relaxed);
        if ns >= 1_000_000 {
            HB_OVER_1MS.fetch_add(1, Relaxed);
        }
        if ns >= 10_000_000 {
            HB_OVER_10MS.fetch_add(1, Relaxed);
        }
        if ns >= 100_000_000 {
            HB_OVER_100MS.fetch_add(1, Relaxed);
        }
    }

    /// Snapshot: (beats, sum_ns, max_ns, over_1ms, over_10ms, over_100ms).
    pub fn heartbeat_snapshot() -> (u64, u64, u64, u64, u64, u64) {
        (
            HB_BEATS.load(Relaxed),
            HB_SUM_NS.load(Relaxed),
            HB_MAX_NS.load(Relaxed),
            HB_OVER_1MS.load(Relaxed),
            HB_OVER_10MS.load(Relaxed),
            HB_OVER_100MS.load(Relaxed),
        )
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

    pub fn report(wall: Duration) {
        let total_ns: u64 = (0..Phase::COUNT)
            .map(|i| STATS[i].nanos.load(Relaxed))
            .sum();
        let peak_mb = peak_rss_bytes() as f64 / 1e6;
        let files = FILES.load(Relaxed);
        let bytes = BYTES.load(Relaxed);
        let mbps = if wall.as_secs_f64() > 0.0 {
            (bytes as f64 / 1e6) / wall.as_secs_f64()
        } else {
            0.0
        };
        let total_cpu_ns = process_cpu_nanos();
        let cores_avg = if wall.as_secs_f64() > 0.0 {
            total_cpu_ns as f64 / 1e9 / wall.as_secs_f64()
        } else {
            0.0
        };
        eprintln!(
            "PERF wall_ms={} peak_rss_mb={:.1} files_measured={} measured_bytes={} measured_mb_per_s={:.1}",
            wall.as_millis(),
            peak_mb,
            files,
            bytes,
            mbps
        );
        eprintln!(
            "PERF_CPU total_cpu_ms={:.1} cores_avg={:.2}",
            total_cpu_ns as f64 / 1e6,
            cores_avg
        );
        // Runtime responsiveness. Counts only — scripts derive percentages
        // (e.g. over_10ms / beats). over_10ms/over_100ms are the real signal;
        // sub-ms lag is mostly timer granularity.
        let (hb_beats, hb_sum_ns, hb_max_ns, hb_1, hb_10, hb_100) = heartbeat_snapshot();
        let hb_mean_us = if hb_beats > 0 {
            hb_sum_ns as f64 / 1000.0 / hb_beats as f64
        } else {
            0.0
        };
        eprintln!(
            "PERF_HEARTBEAT beats={} tick_ms={} mean_us={:.1} max_us={:.1} over_1ms={} over_10ms={} over_100ms={}",
            hb_beats,
            super::HEARTBEAT_TICK_MS,
            hb_mean_us,
            hb_max_ns as f64 / 1000.0,
            hb_1,
            hb_10,
            hb_100
        );
        // Columns: name, elapsed_ms, elapsed_pct, calls, phase_mb, phase_mb_per_s.
        // Elapsed time includes .await wait for async-scoped phases.
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
            eprintln!(
                "PERF_PHASE {},{:.1},{:.1},{},{:.1},{:.1}",
                NAMES[i],
                ns as f64 / 1e6,
                pct,
                STATS[i].calls.load(Relaxed),
                mb,
                phase_mbps
            );
        }
    }
}

#[cfg(feature = "perf-metrics")]
pub use imp::{
    add_bytes, heartbeat_snapshot, peak_rss_bytes, record_file, record_heartbeat, report, Guard,
};

// No-op surface when the feature is off (everything inlines away).
#[cfg(not(feature = "perf-metrics"))]
#[inline(always)]
pub fn add_bytes(_p: Phase, _n: u64) {}
#[cfg(not(feature = "perf-metrics"))]
#[inline(always)]
pub fn record_file(_bytes: u64) {}

/// Zero-sized stand-in for the timing [`imp::Guard`] when the feature is off, so
/// the `phase!` macro binds a real value (no unit-binding lints) and inlines away.
#[cfg(not(feature = "perf-metrics"))]
pub struct Guard;
#[cfg(not(feature = "perf-metrics"))]
impl Guard {
    #[inline(always)]
    pub fn new(_p: Phase) -> Self {
        Guard
    }
}

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
        assert!(
            rss > 1_000_000,
            "peak RSS too small ({rss} bytes) — unit bug?"
        );
        assert!(
            rss < 100_000_000_000,
            "peak RSS implausibly large ({rss} bytes) — unit bug?"
        );
        eprintln!("peak_rss_bytes() = {rss} ({:.1} MB)", rss as f64 / 1e6);
    }

    #[test]
    fn heartbeat_records_lags_and_buckets() {
        let (b0, _, _, _, o10_0, o100_0) = heartbeat_snapshot();
        record_heartbeat(std::time::Duration::from_micros(200)); // < 1ms
        record_heartbeat(std::time::Duration::from_millis(5)); //   >= 1ms
        record_heartbeat(std::time::Duration::from_millis(50)); //  >= 10ms
        record_heartbeat(std::time::Duration::from_millis(500)); // >= 100ms
        let (b1, _, max_ns, _, o10_1, o100_1) = heartbeat_snapshot();
        // This is the only test that records heartbeats, so deltas are
        // attributable even under parallel execution.
        assert_eq!(b1 - b0, 4);
        assert!(o10_1 - o10_0 >= 2, "the 50ms and 500ms beats exceed 10ms");
        assert!(o100_1 - o100_0 >= 1, "the 500ms beat exceeds 100ms");
        assert!(max_ns >= 500_000_000, "max captures the 500ms beat");
    }
}
