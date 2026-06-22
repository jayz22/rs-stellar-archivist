use clap::Error;
use std::env;
use stellar_archivist::cli;

#[tokio::main]
async fn main() {
    #[cfg(feature = "perf-metrics")]
    let wall_start = std::time::Instant::now();
    #[cfg(feature = "perf-metrics")]
    stellar_archivist::metrics::start_sampler();

    // Diagnostic: log tokio runtime metrics + the active-decode gauge every 500ms when
    // SA_RT_METRICS is set. Only compiled under `--cfg tokio_unstable` (uses unstable
    // RuntimeMetrics accessors); normal builds are unaffected.
    #[cfg(tokio_unstable)]
    if std::env::var_os("SA_RT_METRICS").is_some() {
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
                for i in 0..nw {
                    let b = m.worker_total_busy_duration(i);
                    busy += b.saturating_sub(last_busy[i]).as_secs_f64() / el;
                    last_busy[i] = b;
                }
                let inj = m.injection_queue_depth();
                let locq: usize = (0..nw).map(|i| m.worker_local_queue_depth(i)).sum();
                let dec = stellar_archivist::metrics::ACTIVE_DECODES
                    .load(std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "RTM busy_cores={:.1}/{} active_decodes={} runnable_q={} (inject={} local={})",
                    busy, nw, dec, inj + locq, inj, locq
                );
                last = now;
            }
        });
    }

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
