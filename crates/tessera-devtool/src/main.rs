//! `tessera-devtool`: a thin command-line front for [`tessera_devtool`].
//!
//! `tessera-devtool run <command>` runs the command through `/bin/sh -c --` in a
//! headless terminal and prints the final screen. Exit status: the child's
//! own code (`128 + signal` when a signal ended it), `124` when the timeout
//! expired, `125` when the tool itself failed, `2` for a usage error. On
//! timeout only the command's shell is signalled (`SIGHUP`, then `SIGKILL`
//! 200 ms later); see [`tessera_devtool::run::run`].

#![forbid(unsafe_code)]

use std::io::{self, Write as _};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use tessera_devtool::dump::{self, Format};
use tessera_devtool::run::{self, RunOptions};

/// Exit code when the tool itself fails (cannot spawn, cannot write).
const TOOL_FAILURE: u8 = 125;

#[derive(Debug, Parser)]
#[command(version, about = "Developer tools for the Tessera core")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a command in a headless terminal and print the final screen.
    ///
    /// The command runs as `/bin/sh -c -- <COMMAND>`, so quote it as one
    /// argument. The tool waits for it to exit or for the timeout, then
    /// prints the grid and exits with the command's status (124 on timeout).
    Run(RunArgs),
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Command line passed to `/bin/sh -c` (it may start with `-`).
    #[arg(allow_hyphen_values = true)]
    command: String,
    /// Terminal width in columns.
    ///
    /// Memory grows with it: the default 10 000 lines of scrollback are kept
    /// at full width (about 1.25 MiB per line at 65535 columns) even without
    /// `--scrollback`, and `--format cells` builds its whole output in
    /// memory. An inherited `COLUMNS` overrides this for programs that read
    /// it.
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u16).range(1..))]
    cols: u16,
    /// Terminal height in rows.
    ///
    /// An inherited `LINES` overrides this for programs that read it.
    #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u16).range(1..))]
    rows: u16,
    /// Seconds to wait for the command before killing it (fractions allowed).
    ///
    /// On timeout the command's shell gets `SIGHUP`, then `SIGKILL` if it is
    /// still running 200 ms later; processes it started get the hangup the
    /// kernel sends when the shell dies. Deadlines are checked between
    /// ~512-byte slices of output, so at extreme sizes one slice can stretch
    /// the bound by seconds.
    #[arg(long, value_name = "SECS", default_value = "10", value_parser = parse_seconds)]
    timeout: Duration,
    /// Output format.
    #[arg(long, value_enum, default_value_t = FormatArg::Ansi)]
    format: FormatArg,
    /// Print the scrollback history above the screen.
    #[arg(long)]
    scrollback: bool,
    /// Mark the cursor (`|` after its cell in `text`, `<cursor` in `cells`).
    #[arg(long)]
    cursor: bool,
}

/// `--format` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum FormatArg {
    /// Text with SGR colours and attributes, for a real terminal.
    Ansi,
    /// Plain text, one line per row.
    Text,
    /// One line per cell with its markers and style.
    Cells,
}

impl From<FormatArg> for Format {
    fn from(format: FormatArg) -> Self {
        match format {
            FormatArg::Ansi => Self::Ansi,
            FormatArg::Text => Self::Text,
            FormatArg::Cells => Self::Cells,
        }
    }
}

/// Parses a non-negative, finite number of seconds.
fn parse_seconds(value: &str) -> Result<Duration, String> {
    let secs: f64 = value
        .parse()
        .map_err(|err| format!("not a number of seconds: {err}"))?;
    Duration::try_from_secs_f64(secs).map_err(|err| format!("not a valid timeout: {err}"))
}

fn main() -> ExitCode {
    let Cli {
        command: Command::Run(args),
    } = Cli::parse();
    match run_command(&args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("tessera-devtool: {message}");
            ExitCode::from(TOOL_FAILURE)
        }
    }
}

/// Runs `tessera-devtool run` and returns the exit code to report.
fn run_command(args: &RunArgs) -> Result<ExitCode, String> {
    let options = RunOptions {
        command: args.command.clone(),
        cols: args.cols,
        rows: args.rows,
        timeout: args.timeout,
    };
    let outcome = run::run(&options).map_err(|err| err.to_string())?;
    let screen = dump::render(
        outcome.terminal.grid(),
        args.format.into(),
        dump::DumpOptions {
            scrollback: args.scrollback,
            cursor: args.cursor,
        },
    );
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(screen.as_bytes())
        .and_then(|()| stdout.flush())
    {
        // A reader that stopped early (`| head`) is not a failure of the
        // run; the child's status is still the answer.
        Err(err) if err.kind() != io::ErrorKind::BrokenPipe => {
            return Err(format!("cannot write the dump: {err}"));
        }
        Ok(()) | Err(_) => {}
    }
    // Exit statuses are a byte; a status outside it (a wrapper's own
    // convention) keeps its low byte, as the shell reports it.
    let code = u8::try_from(outcome.exit_code() & 0xff).unwrap_or(TOOL_FAILURE);
    Ok(ExitCode::from(code))
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use tessera_devtool::run::DEFAULT_TIMEOUT;

    use super::*;

    fn run_args(argv: &[&str]) -> RunArgs {
        let Cli {
            command: Command::Run(parsed),
        } = Cli::try_parse_from(argv).expect("valid arguments");
        parsed
    }

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_match_the_library() {
        let args = run_args(&["tessera-devtool", "run", "true"]);
        assert_eq!(args.timeout, DEFAULT_TIMEOUT);
        assert_eq!((args.cols, args.rows), (80, 24));
        assert_eq!(args.format, FormatArg::Ansi);
    }

    #[test]
    fn fractional_timeouts_parse_and_negative_ones_do_not() {
        let args = run_args(&["tessera-devtool", "run", "true", "--timeout", "0.25"]);
        assert_eq!(args.timeout, Duration::from_millis(250));
        assert!(parse_seconds("-1").is_err());
        assert!(parse_seconds("inf").is_err());
        assert!(parse_seconds("soon").is_err());
    }

    #[test]
    fn command_may_start_with_a_hyphen() {
        let args = run_args(&["tessera-devtool", "run", "-x", "--cols", "5"]);
        assert_eq!(args.command, "-x");
        assert_eq!(args.cols, 5);
    }
}
