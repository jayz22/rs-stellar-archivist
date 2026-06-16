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
        nanos: AtomicU64,
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

    static STATS: [Stat; Phase::COUNT] = [const { Stat::new() }; Phase::COUNT];
    static FILES: AtomicU64 = AtomicU64::new(0);
    static BYTES: AtomicU64 = AtomicU64::new(0);
    static SAMPLER_STOP: AtomicBool = AtomicBool::new(false);

    /// RAII timer: records elapsed nanos + a call into the phase on drop.
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

    fn write_csvs(wall: Duration) {
        if let Ok(dir) = std::env::var("SA_PERF_OUT") {
            let dir = std::path::PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            let total_ns: u64 = (0..Phase::COUNT).map(|i| STATS[i].nanos.load(Relaxed)).sum();
            let peak_mb = peak_rss_bytes() as f64 / 1e6;
            let files = FILES.load(Relaxed);
            let bytes = BYTES.load(Relaxed);
            let mbps = if wall.as_secs_f64() > 0.0 {
                (bytes as f64 / 1e6) / wall.as_secs_f64()
            } else {
                0.0
            };
            if let Ok(mut f) = std::fs::File::create(dir.join("phases.csv")) {
                let _ = writeln!(f, "phase,total_ms,pct_of_phase_time,calls,mb,mb_per_s");
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
                        "{},{:.3},{:.2},{},{:.3},{:.2}",
                        NAMES[i],
                        ns as f64 / 1e6,
                        pct,
                        STATS[i].calls.load(Relaxed),
                        mb,
                        phase_mbps
                    );
                }
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

    pub fn snapshot(elapsed: Duration) {
        write_csvs(elapsed);
    }

    pub fn rename_prior_csvs() {
        let Ok(dir) = std::env::var("SA_PERF_OUT") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        for name in &["phases.csv", "headline.csv", "timeseries.csv"] {
            let src = dir.join(name);
            if src.exists() {
                let stem = name.trim_end_matches(".csv");
                let prev = dir.join(format!("{stem}.prev.csv"));
                let _ = std::fs::rename(&src, &prev);
            }
        }
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
        for i in 0..Phase::COUNT {
            let ns = STATS[i].nanos.load(Relaxed);
            let pct = if total_ns > 0 {
                ns as f64 * 100.0 / total_ns as f64
            } else {
                0.0
            };
            eprintln!(
                "PERF_PHASE {},{:.1},{:.1},{},{:.1}",
                NAMES[i],
                ns as f64 / 1e6,
                pct,
                STATS[i].calls.load(Relaxed),
                STATS[i].bytes.load(Relaxed) as f64 / 1e6
            );
        }
        write_csvs(wall);
    }
}

#[cfg(feature = "perf-metrics")]
pub use imp::{add_bytes, peak_rss_bytes, record_file, rename_prior_csvs, report, snapshot,
              start_sampler, Guard};

// ── No-op surface when the feature is off (everything inlines away) ───────────
#[cfg(not(feature = "perf-metrics"))]
#[inline(always)]
pub fn add_bytes(_p: Phase, _n: u64) {}
#[cfg(not(feature = "perf-metrics"))]
#[inline(always)]
pub fn record_file(_bytes: u64) {}
#[cfg(not(feature = "perf-metrics"))]
#[inline(always)]
pub fn snapshot(_elapsed: std::time::Duration) {}
#[cfg(not(feature = "perf-metrics"))]
#[inline(always)]
pub fn rename_prior_csvs() {}

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
