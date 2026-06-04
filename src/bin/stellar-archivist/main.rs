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
