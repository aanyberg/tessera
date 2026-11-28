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
//!   the session (at most [`MAX_LOCAL_HOST_NAMES`]: the session's first
//!   and the newest others), as sent (decoded, as fish 4.3 and later
//!   percent-encode it) or as Tessera's scripts send it
//!   (reduced to `[A-Za-z0-9._-]`, so a name such as `Mårten.local` still
//!   matches): a shell sends the name it read when it started (zsh
//!   `$HOST`, bash `$HOSTNAME`), and macOS changes the name with the
//!   network.
//! - While no non-local report is in effect, on output at most once per
//!   [`SSH_PROBE_INTERVAL`]: the program in front is asked for its name
//!   (the terminal's foreground job, or with no job the session's own
//!   program, as for a profile's `command = "ssh build-box"`), and one
//!   called [`SSH_PROCESS_NAME`] makes the session remote without a host
//!   ([`Remote::Unnamed`]), for a remote shell that sends no reports.
//!
//! - When the session's program exits: a dead pane runs nothing remote,
//!   so the session is local again.
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
use tessera_term::shown_in_title;
use unicode_width::UnicodeWidthChar;

use crate::url::is_default_ignorable;

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
/// reports. The first is never forgotten (the shell reports the name it
/// read when it started for as long as it runs); of the others the oldest
/// is forgotten first.
pub const MAX_LOCAL_HOST_NAMES: usize = 4;

/// Where a session's shell runs, as the frontend shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Remote {
    /// On this machine, or nothing says otherwise.
    #[default]
    Local,
    /// On another machine, which the last OSC 7 report named: sanitised
    /// ([`shown_host`]: no control, bidi, line-separator or invisible
    /// characters), with something visible and at most
    /// [`MAX_REMOTE_HOST_BYTES`] bytes.
    Host(String),
    /// On another machine whose name is not shown: the foreground job is
    /// `ssh` and no report named a host, or the report's host could not be
    /// shown (too long, or nothing visible once sanitised). The frontend shows
    /// it as `⇢ ssh`.
    Unnamed,
}

/// `host` as the frontend may show it, or `None` when it is longer than
/// [`MAX_REMOTE_HOST_BYTES`] (measured before sanitising, so the bound is
/// on what the child sent) or nothing visible remains (S3) once
///
/// - the characters a title drops ([`tessera_term::shown_in_title`]:
///   control, bidi-control and line-separator characters) and the
///   default-ignorable (invisible) ones ([`is_default_ignorable`]: zero
///   width spaces and joiners, U+FEFF, the Hangul fillers, tags, variation
///   selectors) are removed, since no host name needs them and they would
///   make one host look like another, and
/// - surrounding whitespace is trimmed.
///
/// Visible means a character that takes a cell: whitespace and combining
/// marks alone are not a host.
#[must_use]
pub fn shown_host(host: &str) -> Option<String> {
    if host.len() > MAX_REMOTE_HOST_BYTES {
        return None;
    }
    let kept: String = host
        .chars()
        .filter(|&c| shown_in_title(c) && !is_default_ignorable(c))
        .collect();
    let trimmed = kept.trim();
    trimmed
        .chars()
        .any(|c| !c.is_whitespace() && c.width().is_some_and(|width| width > 0))
        .then(|| trimmed.to_owned())
}

/// `name` as Tessera's shell integration scripts put it in an OSC 7
/// report (`[A-Za-z0-9._-]` kept, every other byte removed), when that
/// differs from `name` and is not empty (a script sends `localhost` for
/// an empty one, which is local anyway).
fn as_scripts_send(name: &str) -> Option<String> {
    let sent = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
    if name.bytes().all(sent) {
        return None;
    }
    let reduced: String = name.bytes().filter(|&b| sent(b)).map(char::from).collect();
    (!reduced.is_empty()).then_some(reduced)
}

/// A session's remote state, owned by its reader thread.
#[derive(Debug, Default)]
pub(crate) struct RemoteTracker {
    /// This machine's host names during the session, at most
    /// [`MAX_LOCAL_HOST_NAMES`] and none empty: the first one remembered
    /// (the session's start) first and kept, then the others, newest last.
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
    /// but the first beyond [`MAX_LOCAL_HOST_NAMES`]. An empty name
    /// (`gethostname` failed) is not kept; the first name stays first.
    fn remember(&mut self, name: String) {
        if name.is_empty() || self.local_names.front() == Some(&name) {
            return;
        }
        if let Some(known) = self
            .local_names
            .iter()
            .skip(1)
            .position(|known| *known == name)
        {
            self.local_names.remove(known + 1);
        } else if self.local_names.len() == MAX_LOCAL_HOST_NAMES {
            // Never the first (index 0): the oldest of the others.
            self.local_names.remove(1);
        }
        self.local_names.push_back(name);
    }

    /// Whether `cwd` names this machine: by one of the local names, as
    /// sent or as the scripts send it ([`as_scripts_send`]), or with no
    /// local name known by the empty host or `localhost` only.
    fn names_this_machine(&self, cwd: &Cwd) -> bool {
        if self.local_names.is_empty() {
            return cwd.is_local("");
        }
        self.local_names.iter().any(|name| {
            cwd.is_local(name) || as_scripts_send(name).is_some_and(|sent| cwd.is_local(&sent))
        })
    }

    /// Applies a valid OSC 7 report, while this machine is called
    /// `local_hostname`: local when it names this machine by that name or
    /// one it had earlier in the session.
    pub(crate) fn report(&mut self, cwd: &Cwd, local_hostname: String) {
        self.remember(local_hostname);
        if self.names_this_machine(cwd) {
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

    /// The session's program exited: nothing of it runs on another machine
    /// any more, so the session is local (a dead pane keeps no `⇢ ssh`).
    /// Returns the change, as [`changed`](Self::changed) does.
    pub(crate) fn exited(&mut self) -> Option<Remote> {
        self.reported = None;
        self.ssh = false;
        self.changed()
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
        for name in ["first", "", "second", "third", "second"] {
            tracker.report(&at("x"), name.into());
        }
        assert_eq!(
            tracker.local_names,
            ["first", "third", "second"],
            "deduplicated, the first kept first, the others newest last"
        );
        for n in 0..10 {
            tracker.report(&at("x"), format!("name-{n}"));
            assert!(tracker.local_names.len() <= MAX_LOCAL_HOST_NAMES);
        }
        assert_eq!(tracker.local_names, ["first", "name-7", "name-8", "name-9"]);
        assert_eq!(
            report(&mut tracker, "second", "name-9"),
            Some(Remote::Host("second".into())),
            "a forgotten name is another machine's"
        );
        assert_eq!(
            report(&mut tracker, "name-7", "name-9"),
            Some(Remote::Local)
        );
    }

    /// #358: the session's first name is the one its shell keeps sending
    /// (zsh `$HOST` is read once), so however many network names follow it
    /// stays this machine's.
    #[test]
    fn the_first_name_is_never_forgotten() {
        let mut tracker = RemoteTracker::new("Mac.home".into());
        for n in 0..(MAX_LOCAL_HOST_NAMES * 3) {
            assert_eq!(
                report(&mut tracker, "Mac.home", &format!("net-{n}.local")),
                None,
                "after {n} network names"
            );
        }
        tracker.report(&at("x"), "Mac.home".into());
        assert_eq!(
            tracker.local_names.front().map(String::as_str),
            Some("Mac.home")
        );
        // With no name at the start, the first one learnt is kept instead.
        let mut tracker = RemoteTracker::new(String::new());
        for name in ["one", "two", "three", "four", "five", "six"] {
            tracker.report(&at("x"), name.into());
        }
        assert_eq!(tracker.local_names, ["one", "four", "five", "six"]);
    }

    /// #358: the scripts reduce the host to `[A-Za-z0-9._-]`, so a machine
    /// called `Mårten.local` (or a name that is not UTF-8, which
    /// `local_hostname` gives with U+FFFD) reports `Mrten.local`.
    #[test]
    fn a_name_the_scripts_reduce_is_still_this_machine() {
        for (local, sent) in [
            ("M\u{e5}rten.local", "Mrten.local"),
            ("M\u{e5}rten.local", "M\u{e5}rten.local"),
            ("M\u{e5}rten", "mrten.lan"),
            ("M\u{fffd}rten.local", "Mrten.local"),
            ("caf\u{e9} mac", "cafmac"),
        ] {
            let mut tracker = RemoteTracker::new(local.into());
            assert_eq!(
                report(&mut tracker, sent, local),
                None,
                "{sent:?} from {local:?}"
            );
        }
        let mut tracker = RemoteTracker::new("M\u{e5}rten.local".into());
        assert_eq!(
            report(&mut tracker, "Marten.local", "M\u{e5}rten.local"),
            Some(Remote::Host("Marten.local".into())),
            "another name is another machine"
        );
        assert_eq!(as_scripts_send("Mac.local"), None, "already as sent");
        assert_eq!(
            as_scripts_send("\u{e5}\u{e4}\u{f6}"),
            None,
            "sent as localhost"
        );
        assert_eq!(as_scripts_send("a b"), Some("ab".into()));
    }

    #[test]
    fn a_fish_style_encoded_host_is_compared_and_shown_decoded() {
        // fish 4.3 and later send their own OSC 7 with the host
        // percent-encoded (#358).
        let sent = |tracker: &mut RemoteTracker, payload: &[u8], local: &str| {
            let cwd = tessera_shell_integration::osc7::parse_osc7(payload).unwrap();
            tracker.report(&cwd, local.into());
            tracker.changed()
        };
        let mut tracker = RemoteTracker::new("M\u{e5}rten.local".into());
        assert_eq!(
            sent(
                &mut tracker,
                b"file://M%C3%A5rten.local/tmp",
                "M\u{e5}rten.local"
            ),
            None,
            "this machine"
        );
        let mut tracker = RemoteTracker::new("M\u{fffd}rten".into());
        assert_eq!(
            sent(&mut tracker, b"file://M%E5rten/tmp", "M\u{fffd}rten"),
            None,
            "this machine, not UTF-8"
        );
        let mut tracker = RemoteTracker::new("Mac".into());
        assert_eq!(
            sent(&mut tracker, b"file://my%20host/tmp", "Mac"),
            Some(Remote::Host("my host".into()))
        );
        assert_eq!(
            sent(&mut tracker, b"file://%E2%80%8B%EF%BB%BF/tmp", "Mac"),
            Some(Remote::Unnamed),
            "decoded invisible text is not shown"
        );
        // #526 (25.3 r2 M2): a bidi override (U+202E) or isolate (U+2066)
        // that arrives percent-encoded is removed after decoding, as when
        // sent raw; an encoded C1 control (CSI, U+009B) refuses the report.
        assert_eq!(
            sent(&mut tracker, b"file://ev%E2%80%AEil.com/tmp", "Mac"),
            Some(Remote::Host("evil.com".into()))
        );
        assert_eq!(
            sent(&mut tracker, b"file://%E2%80%AE%E2%81%A6/tmp", "Mac"),
            Some(Remote::Unnamed),
            "decoded bidi controls alone are not shown"
        );
        assert_eq!(
            tessera_shell_integration::osc7::parse_osc7(b"file://ev%C2%9Bil.com/tmp"),
            None
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
        // #358: invisible characters are removed, and a host with nothing
        // visible (only invisible characters, combining marks or
        // whitespace) is not shown.
        assert_eq!(
            shown_host("ev\u{200B}il\u{FEFF}.c\u{2060}om\u{E0041}\u{FE0F}"),
            Some("evil.com".into())
        );
        assert_eq!(
            shown_host("e\u{301}x"),
            Some("e\u{301}x".into()),
            "a mark on a letter"
        );
        for invisible in [
            "\u{200B}",
            "\u{FEFF}\u{FEFF}",
            "\u{3164}",
            "\u{115F}\u{1160}",
            "\u{FFA0}",
            "\u{301}\u{302}",
            "\u{20DD}",
            "\u{E0001}\u{E0068}\u{E007F}",
            "\u{180E}",
            "\u{AD}",
            " \u{200B} \u{A0}",
            "\u{34F}\u{FE00}",
        ] {
            assert_eq!(shown_host(invisible), None, "{invisible:?}");
            let mut tracker = RemoteTracker::default();
            assert_eq!(
                report(&mut tracker, invisible, "Mac"),
                Some(Remote::Unnamed),
                "{invisible:?} is remote, without a name"
            );
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

    /// #358: when the session's program exits, a pane that showed a
    /// host or `ssh` is local again; one that was local reports nothing.
    #[test]
    fn an_exit_makes_the_session_local() {
        let start = Instant::now();
        let mut tracker = RemoteTracker::default();
        output(&mut tracker, start, Some(job("ssh")));
        assert_eq!(tracker.exited(), Some(Remote::Local), "the ssh fallback");
        let mut tracker = RemoteTracker::default();
        report(&mut tracker, "far", "Mac");
        assert_eq!(tracker.exited(), Some(Remote::Local), "a reported host");
        assert_eq!(tracker.exited(), None, "only a change");
        let mut tracker = RemoteTracker::default();
        assert_eq!(tracker.exited(), None, "local all along");
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
