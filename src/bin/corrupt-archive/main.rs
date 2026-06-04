use clap::Parser;
use std::path::PathBuf;
use stellar_archivist::corruption;

#[derive(Parser)]
#[command(about = "Corrupt a local archive for repair testing (perf only)")]
struct Args {
    archive: PathBuf,
    #[arg(long, default_value = "all")]
    kinds: String,
    #[arg(long, default_value_t = 10)]
    count: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    #[arg(long)]
    manifest: Option<PathBuf>,
}

fn main() {
    let a = Args::parse();
    let kinds: Vec<String> = if a.kinds == "all" {
        corruption::ALL_KINDS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    } else {
        a.kinds.split(',').map(|s| s.trim().to_string()).collect()
    };
    let m = corruption::run(&a.archive, &kinds, a.count, a.seed);
    eprintln!("corrupt-archive: applied {} corruption(s)", m.items.len());
    if let Some(path) = a.manifest {
        std::fs::write(&path, serde_json::to_string_pretty(&m).unwrap()).expect("write manifest");
    }
}
