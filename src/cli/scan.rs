use crate::cli::{Error, GlobalArgs};
use crate::{
    pipeline::{Pipeline, PipelineConfig},
    scan_operation::ScanOperation,
    storage, utils,
};
use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
pub struct ScanCmd {
    /// Archive URL to scan (http://, https://, file://)
    pub archive: String,

    /// Scan starting from this ledger (will round to nearest checkpoint)
    #[arg(long)]
    pub low: Option<u32>,

    /// Scan up to this checkpoint only
    #[arg(long)]
    pub high: Option<u32>,
}

impl ScanCmd {
    pub async fn run(self, args: GlobalArgs) -> Result<(), Error> {
        info!("Starting scan of {}", self.archive);

        if let Some(low) = self.low {
            info!("Scanning from ledger {} onwards", low);
        }
        if let Some(high) = self.high {
            info!("Scanning up to checkpoint {}", high);
        }

        let prior = crate::cli::resume_prior(&args)?;

        let src_store = storage::from_url_with_config(&self.archive, &args.storage_config)
            .map_err(|e| Error::Other(format!("Failed to create source backend: {e}")))?;

        let pipeline_config = PipelineConfig {
            concurrency: args.concurrency,
            skip_optional: args.skip_optional,
            skip_history_and_buckets: false,
            verify: args.verify,
            storage_config: args.storage_config,
        };

        // Create the scan operation
        let operation = ScanOperation::new(self.low, self.high, pipeline_config.clone());

        let mut pipeline = Pipeline::new(
            operation,
            pipeline_config,
            src_store,
            None,
            args.report_path.clone(),
        );

        if let Some(prior) = prior {
            pipeline.stats().seed_failures(prior).await;
        }

        if let Some(rp) = args.report_path.clone() {
            let cp = std::sync::Arc::new(crate::checkpoint::single_section_checkpointer(
                rp,
                args.checkpoint_interval,
                std::time::Duration::from_secs(30),
                pipeline.stats_arc(),
            ));
            pipeline.set_checkpointer(cp.clone());
            crate::checkpoint::spawn_signal_handler(cp);
        }

        pipeline.run().await.map_err(utils::map_pipeline_error)?;

        Ok(())
    }
}
