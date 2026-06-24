use clap::Error;
use std::env;
use stellar_archivist::cli;

#[tokio::main]
async fn main() {
    #[cfg(feature = "perf-metrics")]
    let wall_start = std::time::Instant::now();

    // Tokio runtime metrics are part of perf diagnostics, but Tokio exposes
    // these accessors only when compiled with `--cfg tokio_unstable`.
    #[cfg(all(feature = "perf-metrics", tokio_unstable))]
    start_runtime_metrics_logger();
    #[cfg(all(feature = "perf-metrics", not(tokio_unstable)))]
    eprintln!(
        "PERF_NOTE tokio_runtime_metrics=disabled reason=missing_tokio_unstable_cfg \
         rebuild_with='RUSTFLAGS=\"--cfg tokio_unstable\" cargo run --features perf-metrics -- ...'"
    );

    // Runtime-responsiveness heartbeat. Needs no tokio_unstable — just a spawned
    // task that times its own wakeups (see metrics::record_heartbeat).
    #[cfg(feature = "perf-metrics")]
    start_heartbeat();

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

#[cfg(all(feature = "perf-metrics", tokio_unstable))]
fn start_runtime_metrics_logger() {
    let handle = tokio::runtime::Handle::current();
    tokio::spawn(async move {
        let m = handle.metrics();
        let nw = m.num_workers();
        let mut last_busy = vec![std::time::Duration::ZERO; nw];
        let mut last = std::time::Instant::now();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let now = std::time::Instant::now();
            let el = now.duration_since(last).as_secs_f64().max(1e-9);
            let mut busy = 0.0;
            for (i, last_busy_worker) in last_busy.iter_mut().enumerate() {
                let b = m.worker_total_busy_duration(i);
                busy += b.saturating_sub(*last_busy_worker).as_secs_f64() / el;
                *last_busy_worker = b;
            }
            let inj = m.global_queue_depth();
            let locq: usize = (0..nw).map(|i| m.worker_local_queue_depth(i)).sum();
            let dec = stellar_archivist::metrics::ACTIVE_DECODES
                .load(std::sync::atomic::Ordering::Relaxed);
            // Max per-worker mean poll time: a rising value means tasks are
            // running long without yielding (CPU-bound work on the worker pool) —
            // the signal that CPU might belong on the blocking pool instead.
            let max_poll_us = (0..nw)
                .map(|i| m.worker_mean_poll_time(i).as_micros() as u64)
                .max()
                .unwrap_or(0);
            eprintln!(
                "RTM busy_cores={:.1}/{} active_decodes={} runnable_q={} (inject={} local={}) max_poll_us={}",
                busy,
                nw,
                dec,
                inj + locq,
                inj,
                locq,
                max_poll_us
            );
            last = now;
        }
    });
}

#[cfg(feature = "perf-metrics")]
fn start_heartbeat() {
    use std::time::{Duration, Instant};
    let tick = Duration::from_millis(stellar_archivist::metrics::HEARTBEAT_TICK_MS);
    tokio::spawn(async move {
        loop {
            let t0 = Instant::now();
            tokio::time::sleep(tick).await;
            stellar_archivist::metrics::record_heartbeat(t0.elapsed().saturating_sub(tick));
        }
    });
}
