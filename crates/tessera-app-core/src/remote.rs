//! Whether a session's shell runs on another machine (#205, ADR 0022
//! D-HOST amendment), for the frontend's `⇢ <host>:` title prefix.
//!
//! The state is decided in the core, on the session's reader thread:
//!
//! - On every OSC 7 working-directory report (at most one decision per
//!   chunk of output: the chunk's last valid report). A report naming this
//!   machine ([`Cwd::is_local`]) makes the session local; one naming
//!   another makes it remote with that host, shown sanitised
//!   ([`shown_host`]). "This machine" is any host name it has had during
//!   the session (at most [`MAX_LOCAL_HOST_NAMES`], the newest kept): a
//!   shell sends the name it read when it started (zsh `$HOST`, bash
//!   `$HOSTNAME`), and macOS changes the name with the network.
//! - While no non-local report is in effect, on output at most once per
//!   [`SSH_PROBE_INTERVAL`]: the program in front is asked for its name
//!   (the terminal's foreground job, or with no job the session's own
//!   program, as for a profile's `command = "ssh build-box"`), and one
//!   called [`SSH_PROCESS_NAME`] makes the session remote without a host
//!   ([`Remote::Unnamed`]), for a remote shell that sends no reports.
//!
//! The tracker reports a state only when it differs from the last one
//! it reported (S6), so a flood of reports costs at most one event per
//! chunk and a steady stream none.
//!
//! The answer is a display hint, never a security decision: the child can
//! name any host it likes, or none.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tessera_shell_integration::Cwd;

/// Most bytes of a reported host the frontend is shown: the longest DNS
/// name. A longer host is dropped whole (never cut), and the session shows
/// as [`Remote::Unnamed`].
pub const MAX_REMOTE_HOST_BYTES: usize = 253;

/// How often, at most, a session's foreground job is checked for `ssh`
/// while it has no non-local OSC 7 report, however much output arrives.
pub const SSH_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// The foreground job name that marks a session remote without a report.
pub const SSH_PROCESS_NAME: &str = "ssh";

/// Most host names of this machine a session remembers as local (S5): the
/// names `gethostname` returned at the session's start and at each of its
/// reports, the oldest forgotten first.
pub const MAX_LOCAL_HOST_NAMES: usize = 4;

/// Where a session's shell runs, as the frontend shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Remote {
    /// On this machine, or nothing says otherwise.
    #[default]
    Local,
    /// On another machine, which the last OSC 7 report named: sanitised
    /// (no control, bidi or line-separator characters), not empty and at
    /// most [`MAX_REMOTE_HOST_BYTES`] bytes.
    Host(String),
    /// On another machine whose name is not shown: the foreground job is
    /// `ssh` and no report named a host, or the report's host could not be
    /// shown (too long, or nothing left once sanitised). The frontend shows
    /// it as `⇢ ssh`.
    Unnamed,
}

/// `host` as the frontend may show it, or `None` when it is longer than
/// [`MAX_REMOTE_HOST_BYTES`] (measured before sanitising, so the bound is
/// on what the child sent) or nothing remains once control (C0, C1, DEL),
/// bidi-control and line-separator characters are removed and surrounding
/// whitespace is trimmed (S3).
#[must_use]
pub fn shown_host(host: &str) -> Option<String> {
    if host.len() > MAX_REMOTE_HOST_BYTES {
        return None;
    }
    let kept: String = host.chars().filter(|&c| shown_in_title(c)).collect();
    let trimmed = kept.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Whether `c` may appear in a title: the set `tessera-term` keeps in an
/// OSC 0 or 2 title (ADR 0022 S3), here for a host the title is built from.
fn shown_in_title(c: char) -> bool {
    !c.is_control()
        && !matches!(
            c,
            '\u{061C}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{2028}'
                | '\u{2029}'
        )
}

/// A session's remote state, owned by its reader thread.
#[derive(Debug, Default)]
pub(crate) struct RemoteTracker {
    /// This machine's host names during the session, newest last, at most
    /// [`MAX_LOCAL_HOST_NAMES`] and none empty.
    local_names: VecDeque<String>,
    /// What the last valid OSC 7 report made the session, while that report
    /// named another machine; `None` after a local report or before any.
    reported: Option<Remote>,
    /// Whether the last probe found `ssh` in the foreground. Cleared by a
    /// local report, which is newer news than the probe.
    ssh: bool,
    /// When the foreground job was last probed.
    last_probe: Option<Instant>,
    /// The state last returned (S6: only changes are reported).
    shown: Remote,
}

impl RemoteTracker {
    /// A tracker for a session starting while this machine is called
    /// `local_hostname`.
    pub(crate) fn new(local_hostname: String) -> Self {
        let mut tracker = Self::default();
        tracker.remember(local_hostname);
        tracker
    }

    /// Adds `name` to the local names as the newest, forgetting the oldest
    /// beyond [`MAX_LOCAL_HOST_NAMES`]. An empty name (`gethostname`
    /// failed) is not kept.
    fn remember(&mut self, name: String) {
        if name.is_empty() {
            return;
        }
        if let Some(known) = self.local_names.iter().position(|known| *known == name) {
            self.local_names.remove(known);
        } else if self.local_names.len() == MAX_LOCAL_HOST_NAMES {
            self.local_names.pop_front();
        }
        self.local_names.push_back(name);
    }

    /// Applies a valid OSC 7 report, while this machine is called
    /// `local_hostname`: local when it names this machine by that name or
    /// one it had earlier in the session.
    pub(crate) fn report(&mut self, cwd: &Cwd, local_hostname: String) {
        self.remember(local_hostname);
        let local = if self.local_names.is_empty() {
            cwd.is_local("")
        } else {
            self.local_names.iter().any(|name| cwd.is_local(name))
        };
        if local {
            self.reported = None;
            self.ssh = false;
        } else {
            self.reported = Some(shown_host(&cwd.host).map_or(Remote::Unnamed, Remote::Host));
        }
    }

    /// Called for each chunk of output: unless a non-local report is in
    /// effect or the last probe was less than [`SSH_PROBE_INTERVAL`] before
    /// `now()`, asks `probe` for the name of the program in front (the
    /// foreground job's, or the session leader's when there is no job).
    /// Neither is called while a non-local report is in effect.
    pub(crate) fn output(
        &mut self,
        now: impl FnOnce() -> Instant,
        probe: impl FnOnce() -> Option<String>,
    ) {
        if self.reported.is_some() {
            return;
        }
        let now = now();
        if self
            .last_probe
            .is_some_and(|last| now.saturating_duration_since(last) < SSH_PROBE_INTERVAL)
        {
            return;
        }
        self.last_probe = Some(now);
        self.ssh = probe().is_some_and(|name| name == SSH_PROCESS_NAME);
    }

    /// The state, when it differs from the one this last returned (S6).
    pub(crate) fn changed(&mut self) -> Option<Remote> {
        let current = match (&self.reported, self.ssh) {
            (Some(reported), _) => reported.clone(),
            (None, true) => Remote::Unnamed,
            (None, false) => Remote::Local,
        };
        (current != self.shown).then(|| {
            self.shown = current.clone();
            current
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn at(host: &str) -> Cwd {
        Cwd {
            host: host.into(),
            path: "/tmp".into(),
        }
    }

    /// Applies a report from `host` and returns the change it made.
    fn report(tracker: &mut RemoteTracker, host: &str, local: &str) -> Option<Remote> {
        tracker.report(&at(host), local.into());
        tracker.changed()
    }

    /// Runs the output step at `now` with a probe finding `found`, and
    /// returns the change it made.
    fn output(tracker: &mut RemoteTracker, now: Instant, found: Option<String>) -> Option<Remote> {
        tracker.output(|| now, || found);
        tracker.changed()
    }

    fn job(name: &str) -> String {
        name.into()
    }

    #[test]
    fn local_reports_leave_the_session_local() {
        for host in ["", "localhost", "LOCALHOST", "Mac", "mac.lan", "Mac.local."] {
            let mut tracker = RemoteTracker::default();
            assert_eq!(report(&mut tracker, host, "Mac"), None, "{host:?} is local");
        }
    }

    #[test]
    fn another_host_is_remote_and_named() {
        // A fresh tracker for each: a tracker remembers the local names it
        // was given.
        for (host, local, why) in [
            ("build.example.com", "Mac", "another machine"),
            (
                "Mac.home",
                "Mac.lan",
                "a domain suffix that differs is another machine",
            ),
            (
                "other",
                "",
                "an unknown local name matches only the empty host and localhost",
            ),
        ] {
            let mut tracker = RemoteTracker::default();
            assert_eq!(
                report(&mut tracker, host, local),
                Some(Remote::Host(host.into())),
                "{why}"
            );
        }
    }

    /// B1: a shell sends the host name it read when it started; once the
    /// machine's name changes (macOS does with the network), that name is
    /// still this machine's.
    #[test]
    fn a_name_this_machine_had_during_the_session_stays_local() {
        let mut tracker = RemoteTracker::new("Mac.home".into());
        assert_eq!(
            report(&mut tracker, "Mac.home", "mac-dev-server.local"),
            None
        );
        assert_eq!(
            report(&mut tracker, "mac-dev-server.local", "Mac.lan"),
            None
        );
        assert_eq!(
            report(&mut tracker, "build.example.com", "Mac.lan"),
            Some(Remote::Host("build.example.com".into())),
            "another machine is still remote"
        );
        assert_eq!(
            report(&mut tracker, "Mac.home", "Mac.lan"),
            Some(Remote::Local)
        );
    }

    #[test]
    fn the_local_names_stay_within_their_cap() {
        let mut tracker = RemoteTracker::new("first".into());
        for name in ["first", "", "second", "first"] {
            tracker.report(&at("x"), name.into());
        }
        assert_eq!(
            tracker.local_names,
            ["second", "first"],
            "deduplicated, newest last"
        );
        for n in 0..10 {
            tracker.report(&at("x"), format!("name-{n}"));
            assert!(tracker.local_names.len() <= MAX_LOCAL_HOST_NAMES);
        }
        assert_eq!(
            tracker.local_names,
            ["name-6", "name-7", "name-8", "name-9"]
        );
        assert_eq!(
            report(&mut tracker, "first", "name-9"),
            Some(Remote::Host("first".into())),
            "a forgotten name is another machine's"
        );
        assert_eq!(
            report(&mut tracker, "name-6", "name-9"),
            Some(Remote::Local)
        );
    }

    #[test]
    fn the_next_local_report_clears_it() {
        let mut tracker = RemoteTracker::default();
        report(&mut tracker, "far", "Mac");
        assert_eq!(report(&mut tracker, "", "Mac"), Some(Remote::Local));
        assert_eq!(report(&mut tracker, "Mac", "Mac"), None, "unchanged");
    }

    #[test]
    fn only_changes_are_reported() {
        let mut tracker = RemoteTracker::default();
        assert!(report(&mut tracker, "far", "Mac").is_some());
        for _ in 0..1000 {
            assert_eq!(report(&mut tracker, "far", "Mac"), None);
        }
        assert_eq!(
            report(&mut tracker, "near", "Mac"),
            Some(Remote::Host("near".into())),
            "another remote host is a change"
        );
    }

    #[test]
    fn hosts_are_sanitised() {
        assert_eq!(
            shown_host("ev\u{1b}]0;x\u{7}il\u{202E}.c\u{9b}om\u{7f}\u{2028}\n"),
            Some("ev]0;xil.com".into())
        );
        assert_eq!(shown_host("  host  "), Some("host".into()));
        for blank in ["", "\u{1b}\u{7}", "\u{2066}\u{200F}", "   \t"] {
            assert_eq!(shown_host(blank), None, "{blank:?}");
        }
        let mut tracker = RemoteTracker::default();
        assert_eq!(
            report(&mut tracker, "\u{1b}\u{7}", "Mac"),
            Some(Remote::Unnamed),
            "a remote report with nothing to show is still remote"
        );
    }

    #[test]
    fn a_host_longer_than_253_bytes_is_dropped_whole() {
        let longest = "a".repeat(MAX_REMOTE_HOST_BYTES);
        assert_eq!(shown_host(&longest), Some(longest.clone()));
        let longer = format!("{longest}b");
        assert_eq!(shown_host(&longer), None, "never cut");
        // Measured before sanitising: control bytes count.
        let padded = format!("{}host", "\u{7}".repeat(MAX_REMOTE_HOST_BYTES));
        assert_eq!(shown_host(&padded), None);
        let mut tracker = RemoteTracker::default();
        assert_eq!(report(&mut tracker, &longer, "Mac"), Some(Remote::Unnamed));
    }

    #[test]
    fn ssh_in_the_foreground_is_remote_without_a_host() {
        let start = Instant::now();
        let mut tracker = RemoteTracker::default();
        assert_eq!(output(&mut tracker, start, Some(job("zsh"))), None);
        let later = start + SSH_PROBE_INTERVAL;
        assert_eq!(
            output(&mut tracker, later, Some(job("ssh"))),
            Some(Remote::Unnamed)
        );
        let after = later + SSH_PROBE_INTERVAL;
        assert_eq!(
            output(&mut tracker, after, None),
            Some(Remote::Local),
            "back at the shell"
        );
        assert_eq!(
            output(&mut tracker, after + SSH_PROBE_INTERVAL, Some(job("sshd"))),
            None,
            "only ssh itself"
        );
    }

    #[test]
    fn a_local_report_clears_the_ssh_fallback() {
        let start = Instant::now();
        let mut tracker = RemoteTracker::default();
        output(&mut tracker, start, Some(job("ssh")));
        assert_eq!(report(&mut tracker, "", "Mac"), Some(Remote::Local));
    }

    #[test]
    fn a_host_report_wins_over_the_fallback_and_stops_the_probe() {
        let start = Instant::now();
        let mut tracker = RemoteTracker::default();
        output(&mut tracker, start, Some(job("ssh")));
        assert_eq!(
            report(&mut tracker, "far", "Mac"),
            Some(Remote::Host("far".into()))
        );
        let probed = Cell::new(0);
        let clock = Cell::new(0);
        for second in 1..10 {
            let now = start + SSH_PROBE_INTERVAL * second;
            tracker.output(
                || {
                    clock.set(clock.get() + 1);
                    now
                },
                || {
                    probed.set(probed.get() + 1);
                    None
                },
            );
            assert_eq!(tracker.changed(), None);
        }
        assert_eq!(probed.get(), 0, "no probe while a host report is in effect");
        assert_eq!(clock.get(), 0, "nor a clock read");
    }

    #[test]
    fn the_probe_runs_at_most_once_per_interval_under_a_flood() {
        let start = Instant::now();
        let mut tracker = RemoteTracker::default();
        let probed = Cell::new(0_u32);
        let step = Duration::from_micros(100);
        // Ten seconds of chunks, one every 100 µs.
        for chunk in 0..100_000 {
            let now = start + step * chunk;
            tracker.output(
                || now,
                || {
                    probed.set(probed.get() + 1);
                    Some(job("ssh"))
                },
            );
        }
        assert_eq!(probed.get(), 10, "one probe per second of output");
    }
}
