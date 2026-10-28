//! `cargo xtask ab`: A/B probes that compare crates at a base revision with
//! the same crates in the working tree, for performance bars and differential
//! fuzzing.
//!
//! `init` extracts the base revision with `git archive` next to a generated
//! probe crate that depends on both copies, under the aliases `base_<crate>`
//! and `head_<crate>` (`tessera-grid` becomes `base_grid` and `head_grid`).
//! The base workspace's version is rewritten to a semver-incompatible one so
//! Cargo accepts two packages of the same name. `bench` runs one case in
//! alternating base and head processes and reports best-of-N wall time
//! together with `instructions retired` and `cycles elapsed` from
//! `/usr/bin/time -l`, which stay meaningful on a loaded machine where wall
//! time does not. `diff` runs a differential case over a range of seeds.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Paths copied from the base revision: enough to build every workspace member.
const ARCHIVE_PATHS: &[&str] = &["Cargo.toml", "Cargo.lock", "crates", "xtask"];

/// The probe's harness: argument parsing, timing and the seeded generator.
const MAIN_TEMPLATE: &str = include_str!("ab_template/main.rs");

/// The cases file the caller edits.
const CASES_TEMPLATE: &str = include_str!("ab_template/cases.rs");

/// Why an `ab` task could not run.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// Reading or writing a file failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// An external command failed.
    #[error("`{command}` failed ({status}){}", if stderr.is_empty() { String::new() } else { format!(":\n{stderr}") })]
    Command {
        /// The command line.
        command: String,
        /// Its exit status.
        status: String,
        /// Its standard error, trimmed.
        stderr: String,
    },
    /// A manifest could not be parsed.
    #[error("{}: {source}", path.display())]
    Manifest {
        /// The manifest.
        path: PathBuf,
        /// The parse error.
        #[source]
        source: toml::de::Error,
    },
    /// The generated probe manifest could not be serialised.
    #[error("serialising the probe manifest: {0}")]
    Serialize(#[from] toml::ser::Error),
    /// The probe directory already has content.
    #[error("{} already exists and is not empty", .0.display())]
    NotEmpty(PathBuf),
    /// A crate name is not a member of the workspace.
    #[error("`{name}` is not a workspace member at {revision}")]
    UnknownCrate {
        /// The crate name.
        name: String,
        /// `HEAD` or the base revision.
        revision: String,
    },
    /// The base workspace has no `[workspace.package] version` to rewrite.
    #[error("{}: no `version` in [workspace.package]", .0.display())]
    NoWorkspaceVersion(PathBuf),
    /// The probe printed something other than its documented output.
    #[error("probe output not understood: {0}")]
    Probe(String),
}

/// One crate under comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Member {
    /// Package name, for example `tessera-grid`.
    name: String,
    /// Directory relative to the workspace root.
    path: PathBuf,
}

impl Member {
    /// The alias suffix: the name without `tessera-`, with `-` as `_`.
    fn short(&self) -> String {
        self.name
            .strip_prefix("tessera-")
            .unwrap_or(&self.name)
            .replace('-', "_")
    }
}

/// Creates the probe in `out` comparing `crates` at `base` with the working
/// tree under `root`, and returns the aliases it declared.
///
/// The probe resolves registry dependencies of both sides with the working
/// tree's `Cargo.lock`, so a base revision that pinned a different version of
/// an external crate is measured with the working tree's version. On failure
/// `out` is removed again so the command can be retried.
pub(crate) fn init(
    root: &Path,
    base: &str,
    crates: &[String],
    out: &Path,
) -> Result<Vec<String>, Error> {
    if fs::read_dir(out).is_ok_and(|mut entries| entries.next().is_some()) {
        return Err(Error::NotEmpty(out.to_path_buf()));
    }
    let result = create_probe(root, base, crates, out);
    if result.is_err() {
        let _ = fs::remove_dir_all(out);
    }
    result
}

/// The body of `init`, run on an empty or missing `out`.
fn create_probe(
    root: &Path,
    base: &str,
    crates: &[String],
    out: &Path,
) -> Result<Vec<String>, Error> {
    let head_members = workspace_members(root)?;
    let selected = crates
        .iter()
        .map(|name| {
            head_members
                .iter()
                .find(|member| &member.name == name)
                .cloned()
                .ok_or_else(|| Error::UnknownCrate {
                    name: name.clone(),
                    revision: "HEAD".to_owned(),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let base_dir = out.join("base");
    let probe_dir = out.join("probe");
    create_dir(&base_dir)?;
    create_dir(&probe_dir.join("src"))?;

    let tar = out.join("base.tar");
    let mut archive = Command::new("git");
    archive
        .arg("-C")
        .arg(root)
        .args(["archive", "--format=tar", "-o"])
        .arg(&tar)
        .arg(base)
        .args(ARCHIVE_PATHS);
    run(&mut archive)?;
    let mut extract = Command::new("tar");
    extract.arg("-xf").arg(&tar).arg("-C").arg(&base_dir);
    run(&mut extract)?;
    fs::remove_file(&tar).map_err(|source| Error::Io { path: tar, source })?;

    let base_members = workspace_members(&base_dir)?;
    for member in &selected {
        if !base_members.contains(member) {
            return Err(Error::UnknownCrate {
                name: member.name.clone(),
                revision: base.to_owned(),
            });
        }
    }

    let head_version = workspace_version(root)?;
    let base_manifest = base_dir.join("Cargo.toml");
    let text = read(&base_manifest)?;
    let rewritten = set_workspace_version(&text, base_version(&head_version))
        .ok_or_else(|| Error::NoWorkspaceVersion(base_manifest.clone()))?;
    write(&base_manifest, &rewritten)?;

    let release = release_profile(root)?;
    let manifest = probe_manifest(root, &base_dir, &selected, release)?;
    write(&probe_dir.join("Cargo.toml"), &manifest)?;
    let lock = root.join("Cargo.lock");
    fs::copy(&lock, probe_dir.join("Cargo.lock"))
        .map_err(|source| Error::Io { path: lock, source })?;

    let aliases: Vec<String> = selected
        .iter()
        .flat_map(|member| {
            let short = member.short();
            [format!("base_{short}"), format!("head_{short}")]
        })
        .collect();
    write(&probe_dir.join("src/main.rs"), MAIN_TEMPLATE)?;
    let list = aliases
        .iter()
        .map(|alias| format!("`{alias}`"))
        .collect::<Vec<_>>()
        .join(", ");
    write(
        &probe_dir.join("src/cases.rs"),
        &CASES_TEMPLATE.replace("{aliases}", &list),
    )?;
    Ok(aliases)
}

/// One process run of a benchmark side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
    /// Wall time of the measured section, from the probe.
    ns: u128,
    /// Checksum of the case's output, from the probe.
    sum: u64,
    /// `instructions retired` for the whole process, when reported.
    instructions: Option<u64>,
    /// `cycles elapsed` for the whole process, when reported.
    cycles: Option<u64>,
}

/// The result of `bench`.
#[derive(Debug)]
pub(crate) struct BenchReport {
    /// The table to print.
    pub(crate) text: String,
    /// `head / base` for the gating metric (instructions net of process
    /// overhead when available, otherwise best wall time).
    pub(crate) ratio: f64,
    /// Reasons the comparison cannot back a performance bar: the sides
    /// computed different results, or the case is too small to measure
    /// against the process overhead.
    pub(crate) problems: Vec<String>,
}

/// The built-in case that does nothing, run to measure process overhead.
const EMPTY_CASE: &str = "__empty";

/// A case must do at least this many times the spread of the empty runs'
/// instruction counts (their run-to-run noise), so the subtracted overhead
/// moves the ratio by no more than about 0.1%.
const MIN_WORK_OVER_NOISE: u64 = 1000;

/// Empty runs needed to estimate the noise; with fewer, a case must do at
/// least the overhead itself in work.
const MIN_EMPTY_RUNS_FOR_NOISE: usize = 3;

/// Builds the probe in `dir` and runs `case` in `pairs` alternating
/// base/head process pairs, each preceded by a run of the empty case to
/// measure the process overhead that `/usr/bin/time` counts.
pub(crate) fn bench(dir: &Path, case: &str, pairs: u32) -> Result<BenchReport, Error> {
    let bin = build(dir)?;
    let mut base = Vec::new();
    let mut head = Vec::new();
    let mut empty = Vec::new();
    for pair in 0..pairs.max(1) {
        let order = if pair % 2 == 0 {
            ["base", "head"]
        } else {
            ["head", "base"]
        };
        empty.push(run_side(&bin, EMPTY_CASE, order[0])?);
        for side in order {
            let sample = run_side(&bin, case, side)?;
            if side == "base" {
                base.push(sample);
            } else {
                head.push(sample);
            }
        }
    }
    Ok(report(case, &base, &head, &empty, &load_averages()))
}

/// Runs one bench process, under `/usr/bin/time -l` when it exists.
fn run_side(bin: &Path, case: &str, side: &str) -> Result<Sample, Error> {
    let time = Path::new("/usr/bin/time");
    let mut command = if time.exists() {
        let mut command = Command::new(time);
        command.arg("-l").arg(bin);
        command
    } else {
        Command::new(bin)
    };
    command.args(["bench", case, side]);
    let output = output(&mut command)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let (ns, sum) =
        parse_probe_line(&stdout).ok_or_else(|| Error::Probe(stdout.trim().to_owned()))?;
    Ok(Sample {
        ns,
        sum,
        instructions: time_field(&stderr, "instructions retired"),
        cycles: time_field(&stderr, "cycles elapsed"),
    })
}

/// Builds the probe in `dir` and runs differential `case` for `count` seeds
/// from `first`. Returns whether every seed agreed; the probe prints the
/// first mismatch.
pub(crate) fn diff(dir: &Path, case: &str, first: u64, count: u64) -> Result<bool, Error> {
    let bin = build(dir)?;
    let mut command = Command::new(&bin);
    command.args(["diff", case, &first.to_string(), &count.to_string()]);
    let status = command.status().map_err(|source| Error::Io {
        path: bin.clone(),
        source,
    })?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Error::Command {
            command: describe(&command),
            status: status.to_string(),
            stderr: String::new(),
        }),
    }
}

/// Builds the probe's release binary and returns its path.
fn build(dir: &Path) -> Result<PathBuf, Error> {
    let target = dir.join("target");
    let mut cargo = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cargo
        .args(["build", "--release", "--quiet", "--manifest-path"])
        .arg(dir.join("probe/Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target);
    let status = cargo.status().map_err(|source| Error::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    if !status.success() {
        return Err(Error::Command {
            command: describe(&cargo),
            status: status.to_string(),
            stderr: String::new(),
        });
    }
    Ok(target.join("release/ab-probe"))
}

/// Reads one metric from a sample.
type Field = fn(&Sample) -> Option<u128>;

/// Formats the comparison table for one case. Instructions and cycles are
/// reported net of the best `empty` run, because `/usr/bin/time` counts the
/// whole process and a fixed startup cost pulls every ratio towards 1.
fn report(
    case: &str,
    base: &[Sample],
    head: &[Sample],
    empty: &[Sample],
    load: &str,
) -> BenchReport {
    let overhead_instructions = empty.iter().filter_map(|s| s.instructions).min();
    let overhead_cycles = empty.iter().filter_map(|s| s.cycles).min();
    let net = |samples: &[Sample]| -> Vec<Sample> {
        samples
            .iter()
            .map(|s| Sample {
                instructions: s
                    .instructions
                    .map(|i| i.saturating_sub(overhead_instructions.unwrap_or(0))),
                cycles: s
                    .cycles
                    .map(|c| c.saturating_sub(overhead_cycles.unwrap_or(0))),
                ..*s
            })
            .collect()
    };
    let (base, head) = (net(base), net(head));

    let mut text = String::new();
    let mut problems = Vec::new();
    let _ = writeln!(text, "case `{case}`, {} pairs, {load}", base.len());
    if let Some(overhead) = overhead_instructions {
        let _ = writeln!(
            text,
            "process overhead (empty case, subtracted): {overhead} instructions, {} cycles",
            overhead_cycles.unwrap_or(0)
        );
    }
    let _ = writeln!(
        text,
        "{:<14} {:>14} {:>14} {:>9} {:>14} {:>14}",
        "metric", "base best", "head best", "head/base", "base median", "head median"
    );
    let rows: [(&str, Field); 3] = [
        ("wall ns", |s| Some(s.ns)),
        ("instructions", |s| s.instructions.map(u128::from)),
        ("cycles", |s| s.cycles.map(u128::from)),
    ];
    let mut wall = None;
    let mut instructions = None;
    for (name, field) in rows {
        let (Some(base_stats), Some(head_stats)) = (stats(&base, field), stats(&head, field))
        else {
            continue;
        };
        let ratio = ratio(head_stats.0, base_stats.0);
        let _ = writeln!(
            text,
            "{name:<14} {:>14} {:>14} {ratio:>9.3} {:>14} {:>14}",
            base_stats.0, head_stats.0, base_stats.1, head_stats.1
        );
        match name {
            "wall ns" => wall = Some(ratio),
            "instructions" => instructions = Some(ratio),
            _ => {}
        }
    }

    let empty_counts: Vec<u64> = empty.iter().filter_map(|s| s.instructions).collect();
    let base_work = base.iter().filter_map(|s| s.instructions).min();
    if let (Some(work), Some(overhead)) = (base_work, overhead_instructions) {
        let floor = if empty_counts.len() >= MIN_EMPTY_RUNS_FOR_NOISE {
            let spread = empty_counts.iter().max().unwrap_or(&overhead) - overhead;
            spread.saturating_mul(MIN_WORK_OVER_NOISE).max(1)
        } else {
            overhead
        };
        if work < floor {
            problems.push(format!(
                "case too small: base does {work} instructions of work, under the floor of {floor} \
                 ({MIN_WORK_OVER_NOISE}x the spread of the empty runs, or the overhead itself with \
                 fewer than {MIN_EMPTY_RUNS_FOR_NOISE} of them)"
            ));
        }
    }
    let mut sums: Vec<u64> = base.iter().chain(&head).map(|s| s.sum).collect();
    sums.sort_unstable();
    sums.dedup();
    if sums.len() > 1 {
        problems.push(format!(
            "checksums differ between runs ({sums:?}); the sides do not compute the same result"
        ));
    }
    for problem in &problems {
        let _ = writeln!(text, "problem: {problem}");
    }
    let gate = if instructions.is_some() {
        "instructions (net of overhead)"
    } else {
        "wall ns"
    };
    let _ = writeln!(text, "gating metric: {gate}");
    BenchReport {
        text,
        ratio: instructions.or(wall).unwrap_or(f64::NAN),
        problems,
    }
}

/// Best (minimum) and median of `field` over `samples`, or `None` when any
/// sample lacks it.
fn stats(samples: &[Sample], field: Field) -> Option<(u128, u128)> {
    let mut values = samples.iter().map(field).collect::<Option<Vec<_>>>()?;
    values.sort_unstable();
    Some((*values.first()?, values[values.len() / 2]))
}

/// `numerator / denominator` as a float, `NaN` for a zero denominator.
#[allow(
    clippy::cast_precision_loss,
    reason = "ratios of counts need no exactness"
)]
fn ratio(numerator: u128, denominator: u128) -> f64 {
    if denominator == 0 {
        f64::NAN
    } else {
        numerator as f64 / denominator as f64
    }
}

/// Parses the probe's `ns=<n> sum=<n>` line.
fn parse_probe_line(stdout: &str) -> Option<(u128, u64)> {
    let line = stdout.lines().find(|line| line.starts_with("ns="))?;
    let mut ns = None;
    let mut sum = None;
    for field in line.split_whitespace() {
        if let Some(value) = field.strip_prefix("ns=") {
            ns = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("sum=") {
            sum = value.parse().ok();
        }
    }
    Some((ns?, sum?))
}

/// Reads one counter from `/usr/bin/time -l` output, such as
/// `   10502368  instructions retired`.
fn time_field(stderr: &str, label: &str) -> Option<u64> {
    stderr.lines().find_map(|line| {
        let line = line.trim();
        let value = line.strip_suffix(label)?;
        value.trim().parse().ok()
    })
}

/// The load averages from `uptime`, for the report header.
fn load_averages() -> String {
    Command::new("uptime")
        .output()
        .ok()
        .and_then(|output| {
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.find("load average")
                .map(|start| text[start..].trim().to_owned())
        })
        .unwrap_or_else(|| "load average unknown".to_owned())
}

/// A version that cannot be semver-compatible with `head`.
fn base_version(head: &str) -> &'static str {
    if head.starts_with("0.0.") {
        "999.0.0"
    } else {
        "0.0.0"
    }
}

/// Replaces `version` in the `[workspace.package]` table of a root manifest,
/// keeping every other line. `None` when the table has no `version`.
fn set_workspace_version(manifest: &str, version: &str) -> Option<String> {
    let mut in_table = false;
    let mut replaced = false;
    let mut out = String::with_capacity(manifest.len());
    for line in manifest.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_table = trimmed == "[workspace.package]";
        } else if in_table
            && !replaced
            && trimmed.split('=').next().map(str::trim) == Some("version")
        {
            let _ = writeln!(out, "version = \"{version}\"");
            replaced = true;
            continue;
        }
        out.push_str(line);
    }
    replaced.then_some(out)
}

/// Reads the root manifest at `root`.
fn root_manifest(root: &Path) -> Result<toml::Table, Error> {
    let path = root.join("Cargo.toml");
    let text = read(&path)?;
    text.parse()
        .map_err(|source| Error::Manifest { path, source })
}

/// `[workspace.package] version` of the workspace at `root`.
fn workspace_version(root: &Path) -> Result<String, Error> {
    root_manifest(root)?
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::NoWorkspaceVersion(root.join("Cargo.toml")))
}

/// The workspace members under `root` with their package names.
fn workspace_members(root: &Path) -> Result<Vec<Member>, Error> {
    let manifest = root_manifest(root)?;
    let members = manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for member in members.iter().filter_map(toml::Value::as_str) {
        let path = root.join(member).join("Cargo.toml");
        let text = read(&path)?;
        let table: toml::Table = text
            .parse()
            .map_err(|source| Error::Manifest { path, source })?;
        if let Some(name) = table
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
        {
            out.push(Member {
                name: name.to_owned(),
                path: PathBuf::from(member),
            });
        }
    }
    Ok(out)
}

/// The root `[profile.release]`, made profiler-friendly: line tables kept,
/// nothing stripped.
fn release_profile(root: &Path) -> Result<toml::Table, Error> {
    let mut profile = root_manifest(root)?
        .get("profile")
        .and_then(|p| p.get("release"))
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    profile.insert("debug".into(), "line-tables-only".into());
    profile.insert("strip".into(), "none".into());
    Ok(profile)
}

/// The probe crate's manifest.
fn probe_manifest(
    root: &Path,
    base_dir: &Path,
    members: &[Member],
    release: toml::Table,
) -> Result<String, Error> {
    let mut package = toml::Table::new();
    package.insert("name".into(), "ab-probe".into());
    package.insert("version".into(), "0.0.0".into());
    package.insert("edition".into(), "2024".into());
    package.insert("publish".into(), false.into());

    let mut dependencies = toml::Table::new();
    for member in members {
        for (prefix, dir) in [("base", base_dir), ("head", root)] {
            let mut dep = toml::Table::new();
            dep.insert("package".into(), member.name.clone().into());
            dep.insert(
                "path".into(),
                dir.join(&member.path).display().to_string().into(),
            );
            dependencies.insert(format!("{prefix}_{}", member.short()), dep.into());
        }
    }

    let mut profile = toml::Table::new();
    profile.insert("release".into(), release.into());

    let mut manifest = toml::Table::new();
    manifest.insert("package".into(), package.into());
    manifest.insert("workspace".into(), toml::Table::new().into());
    manifest.insert("dependencies".into(), dependencies.into());
    manifest.insert("profile".into(), profile.into());
    Ok(toml::to_string(&manifest)?)
}

/// Runs `command`, failing on a non-zero exit.
fn run(command: &mut Command) -> Result<(), Error> {
    output(command).map(drop)
}

/// Runs `command` and captures its output, failing on a non-zero exit.
fn output(command: &mut Command) -> Result<std::process::Output, Error> {
    let output = command.output().map_err(|source| Error::Io {
        path: PathBuf::from(command.get_program()),
        source,
    })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(Error::Command {
            command: describe(command),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// A command line for error messages.
fn describe(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Creates `path` and its parents.
fn create_dir(path: &Path) -> Result<(), Error> {
    fs::create_dir_all(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Reads `path` as UTF-8.
fn read(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Writes `contents` to `path`.
fn write(path: &Path, contents: &str) -> Result<(), Error> {
    fs::write(path, contents).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ns: u128, instructions: Option<u64>) -> Sample {
        Sample {
            ns,
            sum: 7,
            instructions,
            cycles: instructions.map(|i| i / 2),
        }
    }

    #[test]
    fn reads_the_macos_time_counters() {
        let stderr = "        0.00 real         0.00 user         0.00 sys\n\
                      \x20            1196032  maximum resident set size\n\
                      \x20           10502368  instructions retired\n\
                      \x20            4675299  cycles elapsed\n";
        assert_eq!(time_field(stderr, "instructions retired"), Some(10_502_368));
        assert_eq!(time_field(stderr, "cycles elapsed"), Some(4_675_299));
        assert_eq!(time_field(stderr, "page faults"), None);
    }

    #[test]
    fn reads_the_probe_line_and_rejects_others() {
        assert_eq!(
            parse_probe_line("warming up\nns=1500 sum=42\n"),
            Some((1500, 42))
        );
        assert_eq!(parse_probe_line("ns=1500\n"), None);
        assert_eq!(parse_probe_line("ns=x sum=1\n"), None);
        assert_eq!(parse_probe_line(""), None);
    }

    #[test]
    fn rewrites_only_the_workspace_package_version() {
        let manifest = "[package]\nversion = \"9.9.9\"\n\n[workspace.package]\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nversion = \"x\"\n";
        let out = set_workspace_version(manifest, "0.0.0").unwrap();
        assert_eq!(
            out,
            "[package]\nversion = \"9.9.9\"\n\n[workspace.package]\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nversion = \"x\"\n"
        );
        assert_eq!(
            set_workspace_version("[workspace]\nmembers = []\n", "0.0.0"),
            None
        );
    }

    #[test]
    fn base_version_is_never_compatible_with_head() {
        assert_eq!(base_version("0.1.0"), "0.0.0");
        assert_eq!(base_version("1.2.3"), "0.0.0");
        assert_eq!(base_version("0.0.0"), "999.0.0");
        assert_eq!(base_version("0.0.4"), "999.0.0");
    }

    #[test]
    fn report_gates_on_instructions_and_falls_back_to_wall_time() {
        let base = [
            sample(200, Some(1000)),
            sample(100, Some(900)),
            sample(300, Some(950)),
        ];
        let head = [
            sample(150, Some(1800)),
            sample(160, Some(1900)),
            sample(400, Some(2000)),
        ];
        let with_counters = report("ascii", &base, &head, &[], "load average: 1");
        assert!(
            (with_counters.ratio - 2.0).abs() < 1e-9,
            "{}",
            with_counters.text
        );
        assert!(with_counters.text.contains("gating metric: instructions"));
        assert!(
            with_counters.problems.is_empty(),
            "{:?}",
            with_counters.problems
        );

        let base = [sample(100, None), sample(120, None)];
        let head = [sample(150, None), sample(130, None)];
        let wall_only = report("ascii", &base, &head, &[], "load average: 1");
        assert!((wall_only.ratio - 1.3).abs() < 1e-9, "{}", wall_only.text);
        assert!(wall_only.text.contains("gating metric: wall ns"));
        assert_eq!(wall_only.problems, Vec::<String>::new());
    }

    #[test]
    fn report_subtracts_process_overhead_before_the_ratio() {
        // Head does twice the work of base on top of a fixed 14M startup cost:
        // the gross ratio would read 1.5, the net one must read 2.
        let overhead = 14_000_000;
        let base = [sample(100, Some(overhead + 14_000_000))];
        let head = [sample(200, Some(overhead + 28_000_000))];
        let quiet = [
            sample(1, Some(overhead + 5_000)),
            sample(1, Some(overhead)),
            sample(1, Some(overhead + 2_000)),
        ];
        let net = report("x", &base, &head, &quiet, "");
        assert!((net.ratio - 2.0).abs() < 1e-9, "{}", net.text);
        assert!(net.text.contains("process overhead"), "{}", net.text);
        assert_eq!(net.problems, Vec::<String>::new());
    }

    #[test]
    fn report_rejects_cases_drowned_by_overhead_noise() {
        let overhead = 14_000_000;
        let base = [sample(100, Some(overhead + 14_000_000))];
        let head = [sample(100, Some(overhead + 14_000_000))];
        // A 50k spread needs 50M instructions of work.
        let noisy = [
            sample(1, Some(overhead + 50_000)),
            sample(1, Some(overhead)),
            sample(1, Some(overhead + 10_000)),
        ];
        let small = report("x", &base, &head, &noisy, "");
        assert!(
            small.problems.iter().any(|p| p.contains("too small")),
            "{:?}",
            small.problems
        );

        // With fewer than three empty runs the floor is the overhead itself.
        let few = [sample(1, Some(overhead))];
        let tiny = [sample(100, Some(overhead + 1_000))];
        assert_eq!(report("x", &tiny, &tiny, &few, "").problems.len(), 1);
        assert_eq!(
            report("x", &base, &head, &few, "").problems,
            Vec::<String>::new()
        );
    }

    #[test]
    fn report_flags_sides_that_compute_different_results() {
        let base = [sample(100, None)];
        let mut other = sample(100, None);
        other.sum = 8;
        let differing = report("x", &base, &[other], &[], "");
        let found = differing
            .problems
            .iter()
            .any(|p| p.contains("checksums differ"));
        assert!(found, "{}", differing.text);
    }

    #[test]
    fn init_builds_a_probe_against_head_and_refuses_a_used_directory() {
        let root = crate::repo_root();
        let out = std::env::temp_dir().join(format!("xtask-ab-{}", std::process::id()));
        let _ = fs::remove_dir_all(&out);

        let aliases = init(&root, "HEAD", &["tessera-vt".to_owned()], &out).unwrap();
        assert_eq!(aliases, ["base_vt", "head_vt"]);

        let manifest: toml::Table = read(&out.join("probe/Cargo.toml"))
            .unwrap()
            .parse()
            .unwrap();
        let deps = manifest["dependencies"].as_table().unwrap();
        assert_eq!(deps["base_vt"]["package"].as_str(), Some("tessera-vt"));
        assert!(
            deps["base_vt"]["path"]
                .as_str()
                .unwrap()
                .ends_with("base/crates/tessera-vt"),
            "{deps:?}"
        );
        assert_eq!(
            deps["head_vt"]["path"].as_str().map(PathBuf::from),
            Some(root.join("crates/tessera-vt"))
        );
        assert_eq!(
            manifest["profile"]["release"]["strip"].as_str(),
            Some("none")
        );
        assert_eq!(workspace_version(&out.join("base")).unwrap(), "0.0.0");
        assert!(out.join("probe/Cargo.lock").is_file());
        assert!(
            read(&out.join("probe/src/cases.rs"))
                .unwrap()
                .contains("`base_vt`, `head_vt`")
        );
        assert!(!out.join("base.tar").exists());

        assert!(matches!(
            init(&root, "HEAD", &["tessera-vt".to_owned()], &out),
            Err(Error::NotEmpty(_))
        ));
        fs::remove_dir_all(&out).unwrap();

        assert!(matches!(
            init(&root, "HEAD", &["no-such-crate".to_owned()], &out),
            Err(Error::UnknownCrate { .. })
        ));
        let _ = fs::remove_dir_all(&out);
    }
}
