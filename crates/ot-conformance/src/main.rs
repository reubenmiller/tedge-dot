//! `ot-conformance` binary: run the OT connector conformance suite from a manifest.
//!
//! ```sh
//! # Layers 1 & 2 — fast, no simulator:
//! ot-conformance check --spec ./conformance.toml --static
//!
//! # Layer 3 — with the protocol simulator + a test broker:
//! ot-conformance check --spec ./conformance.toml --behavioural
//!
//! # Everything (the default):
//! ot-conformance check --spec ./conformance.toml --junit report.xml --json report.json
//! ```
//!
//! Exits non-zero on any failing check.

use clap::{Args, Parser, Subcommand};
use ot_conformance::{manifest::Manifest, run_suite, Selection};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "ot-conformance", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the conformance suite against a connector's conformance.toml.
    Check(CheckArgs),
}

#[derive(Args)]
struct CheckArgs {
    /// Path to the connector's conformance manifest.
    #[arg(long, value_name = "conformance.toml")]
    spec: PathBuf,
    /// Run only layers 1–2 (schema examples + golden decode vectors; no simulator).
    #[arg(long = "static")]
    static_only: bool,
    /// Run only layer 3 (behavioural: connector ⇄ simulator ⇄ test broker).
    #[arg(long = "behavioural", conflicts_with = "static_only")]
    behavioural_only: bool,
    /// Override the SDK's builtin golden vector file (JSON).
    #[arg(long, value_name = "vectors.json")]
    vectors: Option<PathBuf>,
    /// Write a JUnit XML report here.
    #[arg(long, value_name = "report.xml")]
    junit: Option<PathBuf>,
    /// Write a JSON report here.
    #[arg(long, value_name = "report.json")]
    json: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Check(args) => match check(args).await {
            Ok(conformant) => {
                if conformant {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
    }
}

async fn check(args: CheckArgs) -> Result<bool, String> {
    let manifest = Manifest::load(&args.spec)?;
    let selection = Selection {
        static_layers: !args.behavioural_only,
        behavioural: !args.static_only,
    };
    let vectors_text = match &args.vectors {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| format!("failed to read vectors '{}': {e}", path.display()))?,
        ),
        None => None,
    };

    let report = run_suite(&manifest, selection, vectors_text.as_deref()).await?;
    print!("{}", report.render_text());

    if let Some(path) = &args.junit {
        report.write_junit(path)?;
        eprintln!("junit report written to {}", path.display());
    }
    if let Some(path) = &args.json {
        report.write_json(path)?;
        eprintln!("json report written to {}", path.display());
    }
    Ok(report.conformant())
}
