use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;

use scip_php::indexer::Indexer;
use scip_php::project::PhpProject;

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
    let start = Instant::now();

    let project = PhpProject::discover(Path::new(&args.project_root))?;
    eprintln!(
        "scip-php: project '{}' v{} at {}",
        project.package.name,
        project.package.version,
        project.root.display()
    );

    let indexer = Indexer::new(project);
    let index = indexer.index()?;

    let doc_count = index.documents.len();
    let occ_count: usize = index.documents.iter().map(|d| d.occurrences.len()).sum();
    let sym_count: usize = index.documents.iter().map(|d| d.symbols.len()).sum();

    scip::write_message_to_file(&args.output, index)
        .map_err(|e| anyhow::anyhow!("Failed to write SCIP index: {}", e))?;

    let elapsed = start.elapsed();
    eprintln!(
        "scip-php: indexed {} documents, {} occurrences, {} symbols in {:.2}s -> {}",
        doc_count,
        occ_count,
        sym_count,
        elapsed.as_secs_f64(),
        args.output,
    );

    Ok(())
}
