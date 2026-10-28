//! Repository developer tasks, run as `cargo xtask <task>`.
//!
//! The `xtask` alias lives in `.cargo/config.toml`. Each task is one variant
//! of `Task`; run `cargo xtask --help` for the list.
//!
//! Exit status: 0 on success, 1 when a `--check` finds a generated file out of
//! date, 2 when the task itself fails (an I/O or schema error) or the command
//! line is invalid.

#![forbid(unsafe_code)]

mod ab;
mod config_docs;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Command line of `cargo xtask`.
#[derive(Debug, Parser)]
#[command(name = "cargo xtask", about = "Tessera repository tasks")]
struct Cli {
    /// The task to run.
    #[command(subcommand)]
    task: Task,
}

/// One repository task.
#[derive(Debug, Subcommand)]
enum Task {
    /// Generate `docs/config.md` from the `tessera-config` types (doc comments and defaults).
    ConfigDocs {
        /// Do not write: exit 1 with a unified diff if `docs/config.md` is out of date.
        #[arg(long)]
        check: bool,
    },
    /// Compare crates at a base revision with the working tree (benchmarks and differential fuzzing).
    Ab {
        /// What to do with the probe.
        #[command(subcommand)]
        action: AbAction,
    },
}

/// One `cargo xtask ab` action.
#[derive(Debug, Subcommand)]
enum AbAction {
    /// Create a probe crate in `--out` that depends on each crate at `--base` and in the working tree.
    Init {
        /// The base revision, for example `main` or `$(git merge-base main HEAD)`.
        #[arg(long, default_value = "main")]
        base: String,
        /// Workspace crates to compare, for example `tessera-grid`.
        #[arg(long = "crate", required = true, value_name = "NAME")]
        crates: Vec<String>,
        /// Directory for the probe; must not exist or be empty (use a scratch directory).
        #[arg(long)]
        out: PathBuf,
    },
    /// Run a bench case in alternating base/head processes and print best-of-N counters.
    Bench {
        /// The probe directory given to `init --out`.
        dir: PathBuf,
        /// The case name in the probe's `cases.rs`.
        #[arg(long)]
        case: String,
        /// Number of base/head process pairs.
        #[arg(long, default_value_t = 15)]
        pairs: u32,
        /// Exit 1 when head/base on the gating metric exceeds this ratio, the case is too small, or the sides' checksums differ.
        #[arg(long)]
        max_ratio: Option<f64>,
    },
    /// Run a differential case over a seed range; exit 1 on the first mismatch.
    Diff {
        /// The probe directory given to `init --out`.
        dir: PathBuf,
        /// The case name in the probe's `cases.rs`.
        #[arg(long)]
        case: String,
        /// First seed.
        #[arg(long, default_value_t = 0)]
        first: u64,
        /// Number of seeds.
        #[arg(long, default_value_t = 100_000)]
        seeds: u64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = repo_root();
    match cli.task {
        Task::ConfigDocs { check } => run_config_docs(&root, check),
        Task::Ab { action } => run_ab(&root, action),
    }
}

/// Runs an `ab` action and maps its outcome onto the process exit status.
fn run_ab(root: &Path, action: AbAction) -> ExitCode {
    let result = match action {
        AbAction::Init { base, crates, out } => {
            ab::init(root, &base, &crates, &out).map(|aliases| {
                println!(
                    "probe created in {}: write cases in {}/probe/src/cases.rs using {}",
                    out.display(),
                    out.display(),
                    aliases.join(", ")
                );
                ExitCode::SUCCESS
            })
        }
        AbAction::Bench {
            dir,
            case,
            pairs,
            max_ratio,
        } => ab::bench(&dir, &case, pairs).map(|report| {
            print!("{}", report.text);
            match max_ratio {
                Some(_) if !report.problems.is_empty() => {
                    eprintln!("error: the comparison cannot back a bar (see `problem:` above)");
                    ExitCode::from(1)
                }
                Some(max) if report.ratio.is_nan() || report.ratio > max => {
                    eprintln!("error: head/base {:.3} exceeds the bar {max}", report.ratio);
                    ExitCode::from(1)
                }
                _ => ExitCode::SUCCESS,
            }
        }),
        AbAction::Diff {
            dir,
            case,
            first,
            seeds,
        } => ab::diff(&dir, &case, first, seeds).map(|agreed| {
            if agreed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }),
    };
    result.unwrap_or_else(|error| {
        eprintln!("error: ab: {error}");
        ExitCode::from(2)
    })
}

/// Runs `config-docs` and maps its outcome onto the process exit status.
fn run_config_docs(root: &Path, check: bool) -> ExitCode {
    match config_docs::run(root, check) {
        Ok(config_docs::Outcome::Written) => {
            println!("wrote {}", config_docs::DOC_PATH);
            ExitCode::SUCCESS
        }
        Ok(config_docs::Outcome::Unchanged) => {
            println!("{} is already up to date", config_docs::DOC_PATH);
            ExitCode::SUCCESS
        }
        Ok(config_docs::Outcome::UpToDate) => {
            println!("{} is up to date", config_docs::DOC_PATH);
            ExitCode::SUCCESS
        }
        Ok(config_docs::Outcome::Stale { diff }) => {
            eprint!("{diff}");
            eprintln!(
                "error: {} is out of date with crates/tessera-config; run `cargo xtask config-docs` (or `just config-docs`) and commit the result",
                config_docs::DOC_PATH
            );
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("error: config-docs: {error}");
            ExitCode::from(2)
        }
    }
}

/// The workspace root: the parent of this crate's manifest directory.
fn repo_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.parent().unwrap_or(manifest_dir).to_path_buf()
}
