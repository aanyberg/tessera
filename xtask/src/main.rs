//! Repository developer tasks, run as `cargo xtask <task>`.
//!
//! The `xtask` alias lives in `.cargo/config.toml`. Each task is one variant
//! of `Task`; run `cargo xtask --help` for the list.
//!
//! Exit status: 0 on success, 1 when a `--check` finds a generated file out of
//! date, 2 when the task itself fails (an I/O or schema error) or the command
//! line is invalid.

#![forbid(unsafe_code)]

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = repo_root();
    match cli.task {
        Task::ConfigDocs { check } => run_config_docs(&root, check),
    }
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
