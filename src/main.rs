use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "scip-php", about = "SCIP indexer for PHP")]
struct Args {
    /// Path to the PHP project root (containing composer.json)
    #[arg(default_value = ".")]
    project_root: String,

    /// Output file path
    #[arg(short, long, default_value = "index.scip")]
    output: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    println!(
        "scip-php: indexing {} -> {}",
        args.project_root, args.output
    );
    Ok(())
}
