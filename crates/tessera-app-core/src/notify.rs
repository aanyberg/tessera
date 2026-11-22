//! Whether a finished command posts a system notification (#470, ADR 0048).
//!
//! The frontend learns of every [`SessionEvent::CommandFinished`] and knows
//! whether the session's pane has the keyboard; the core decides here,
//! against the configuration in effect, whether that finish marks the tab
//! and whether it also posts, through one app-wide [`NotifyLimiter`] (S6).
//! Nothing here sees command text: the core records none (ADR 0021, 0039).
//!
//! [`SessionEvent::CommandFinished`]: crate::SessionEvent::CommandFinished

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tessera_config::{CommandFinishedMode, Notifications};

/// Most notifications posted in any [`NOTIFY_WINDOW`] (docs/security.md S5,
/// S6). Finishes past it still mark their tab.
pub const MAX_NOTIFICATIONS_PER_MINUTE: usize = 10;

/// The sliding window [`MAX_NOTIFICATIONS_PER_MINUTE`] counts over.
pub const NOTIFY_WINDOW: Duration = Duration::from_secs(60);

/// Most bytes of the tab title a notification shows (S3); the frontend
/// cuts on a character boundary.
pub const MAX_NOTIFICATION_TEXT_BYTES: usize = 256;

/// What the frontend knows about one finished command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandFinishedFacts {
    /// From its C mark to its D mark (or whatever ended it).
    pub duration: Duration,
    /// Whether a `CommandStarted` preceded it. Always true for the core's
    /// events (a finish needs a running command); kept as a guard.
    pub started_seen: bool,
    /// Whether its pane had the keyboard when it finished (ADR 0048 D1).
    pub focused: bool,
}

/// Whether a finish qualifies: the mode is `unfocused`, the pane did not
/// have the keyboard, a start was seen and it ran at least
/// `command_finished_after` seconds (`>=`, comparing durations exactly).
/// The exit status never matters.
#[must_use]
pub fn should_notify(facts: &CommandFinishedFacts, cfg: &Notifications) -> bool {
    cfg.command_finished == CommandFinishedMode::Unfocused
        && facts.started_seen
        && !facts.focused
        && facts.duration >= Duration::from_secs(u64::from(cfg.command_finished_after))
}

/// What the frontend does about a finish, carried as `uint8_t` across the
/// FFI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NotifyDecision {
    /// Neither marks nor posts.
    Nothing = 0,
    /// Marks the pane, posts nothing: the limiter dropped it (D7).
    Mark = 1,
    /// Marks the pane and posts a notification.
    MarkAndPost = 2,
}

/// [`should_notify`], then (only for a finish that qualifies, L10) the
/// limiter at `now`.
pub fn decide(
    facts: &CommandFinishedFacts,
    cfg: &Notifications,
    limiter: &mut NotifyLimiter,
    now: Instant,
) -> NotifyDecision {
    if !should_notify(facts, cfg) {
        NotifyDecision::Nothing
    } else if limiter.allow(now) {
        NotifyDecision::MarkAndPost
    } else {
        NotifyDecision::Mark
    }
}

/// At most [`MAX_NOTIFICATIONS_PER_MINUTE`] posts in any [`NOTIFY_WINDOW`]:
/// a sliding window over the instants of the posts it allowed, at most that
/// many of them stored. The clock is the caller's.
#[derive(Debug, Default)]
pub struct NotifyLimiter {
    posts: VecDeque<Instant>,
    /// Posts refused since the last one allowed.
    dropped: u64,
}

impl NotifyLimiter {
    /// An empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a post at `now` is allowed; records it if so. A `now`
    /// earlier than a stored instant counts as that instant (no negative
    /// time). The first post allowed after some were refused logs their
    /// count once, and nothing else.
    pub fn allow(&mut self, now: Instant) -> bool {
        while let Some(&oldest) = self.posts.front() {
            if now.saturating_duration_since(oldest) >= NOTIFY_WINDOW {
                self.posts.pop_front();
            } else {
                break;
            }
        }
        if self.posts.len() >= MAX_NOTIFICATIONS_PER_MINUTE {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        if self.dropped > 0 {
            tracing::warn!(
                target: "tessera_app_core",
                dropped = self.dropped,
                "command notifications dropped by the rate limit"
            );
            self.dropped = 0;
        }
        self.posts.push_back(now);
        true
    }

    /// Posts refused since the last one allowed.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Instants stored now (at most [`MAX_NOTIFICATIONS_PER_MINUTE`]).
    #[must_use]
    pub fn stored(&self) -> usize {
        self.posts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: CommandFinishedMode, after: u32) -> Notifications {
        Notifications {
            command_finished: mode,
            command_finished_after: after,
        }
    }

    fn facts(duration: Duration, started_seen: bool, focused: bool) -> CommandFinishedFacts {
        CommandFinishedFacts {
            duration,
            started_seen,
            focused,
        }
    }

    const ON: CommandFinishedMode = CommandFinishedMode::Unfocused;
    const OFF: CommandFinishedMode = CommandFinishedMode::Off;

    /// P01-P16. The exit status is not an input at all: P13-P16 (unknown,
    /// 0, 130, -1) are the same fact to `should_notify`.
    #[test]
    fn should_notify_rows() {
        let s = Duration::from_secs;
        let ms = Duration::from_millis;
        let rows = [
            ("P01", ON, 10, s(11), true, false, true),
            ("P02", ON, 10, s(10), true, false, true),
            ("P03", ON, 10, ms(9_999), true, false, false),
            (
                "P03ns",
                ON,
                10,
                s(10).checked_sub(Duration::from_nanos(1)).unwrap(),
                true,
                false,
                false,
            ),
            ("P04", ON, 10, s(9), true, false, false),
            ("P05", ON, 10, s(3600), true, true, false),
            ("P06", OFF, 10, s(3600), true, false, false),
            ("P07", ON, 10, s(3600), false, false, false),
            ("P08", ON, 1, s(1), true, false, true),
            ("P09", ON, 86_400, ms(86_399_999), true, false, false),
            ("P10", ON, 86_400, s(86_400), true, false, true),
            ("P11", ON, 10, Duration::ZERO, true, false, false),
            ("P12", ON, 10, Duration::MAX, true, false, true),
            ("P13-16", ON, 10, s(11), true, false, true),
        ];
        for (id, mode, after, duration, started, focused, expected) in rows {
            assert_eq!(
                should_notify(&facts(duration, started, focused), &cfg(mode, after)),
                expected,
                "{id}"
            );
        }
    }

    /// L01, L02: a flood of 10 000 qualifying finishes in one second posts
    /// exactly 10, and the drops are logged once, as a count, when the
    /// window opens again.
    #[test]
    fn a_flood_posts_ten_and_logs_one_count() {
        let start = Instant::now();
        let mut limiter = NotifyLimiter::new();
        let warnings = crate::session::tests::warnings(|| {
            let allowed = (0..10_000u64)
                .filter(|i| limiter.allow(start + Duration::from_micros(i * 100)))
                .count();
            assert_eq!(allowed, 10);
            assert_eq!(limiter.dropped(), 9_990);
            assert_eq!(limiter.stored(), 10, "S5: memory bounded by the cap");
            assert!(limiter.allow(start + NOTIFY_WINDOW));
        });
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("dropped=9990"), "{warnings:?}");
        assert_eq!(limiter.dropped(), 0);
    }

    /// L03, L04: the window is 60 s exactly.
    #[test]
    fn the_window_slides_at_sixty_seconds() {
        let start = Instant::now();
        let mut limiter = NotifyLimiter::new();
        for _ in 0..10 {
            assert!(limiter.allow(start));
        }
        assert!(!limiter.allow(start + Duration::from_millis(59_999)), "L03");
        assert!(limiter.allow(start + NOTIFY_WINDOW), "L04");
    }

    /// L05, L06: steady use under the cap drops nothing; one every 5 s
    /// drops the 11th within a minute and recovers.
    #[test]
    fn steady_posts() {
        let start = Instant::now();
        let mut limiter = NotifyLimiter::new();
        assert!(
            (0..43u64).all(|i| limiter.allow(start + Duration::from_secs(i * 7))),
            "L05"
        );

        let mut limiter = NotifyLimiter::new();
        let results: Vec<bool> = (0..60u64)
            .map(|i| limiter.allow(start + Duration::from_secs(i * 5)))
            .collect();
        assert!(results[..10].iter().all(|&a| a), "0..45 s");
        assert!(!results[10], "the 11th at 50 s");
        assert!(!results[11], "55 s");
        assert!(results[12], "60 s");
        // Never more than ten in any 60 s window.
        for i in 0..results.len() {
            let window = results[i..(i + 12).min(results.len())]
                .iter()
                .filter(|&&a| a)
                .count();
            assert!(window <= MAX_NOTIFICATIONS_PER_MINUTE, "window at {i}");
        }
    }

    /// L07: a clock that goes backwards neither panics nor frees a slot.
    #[test]
    fn a_clock_that_goes_backwards() {
        let start = Instant::now() + Duration::from_secs(120);
        let mut limiter = NotifyLimiter::new();
        for _ in 0..10 {
            assert!(limiter.allow(start));
        }
        assert!(!limiter.allow(start.checked_sub(Duration::from_secs(100)).unwrap()));
        assert!(!limiter.allow(start + Duration::from_secs(59)));
        assert!(limiter.allow(start + Duration::from_secs(60)));
    }

    /// L08: two sessions share the app-wide limiter: 6 + 6 in one second
    /// post 10 in all.
    #[test]
    fn two_sessions_share_the_limiter() {
        let start = Instant::now();
        let mut limiter = NotifyLimiter::new();
        let on = cfg(ON, 1);
        let finish = facts(Duration::from_secs(2), true, false);
        let decisions: Vec<NotifyDecision> = (0..12u64)
            .map(|i| {
                decide(
                    &finish,
                    &on,
                    &mut limiter,
                    start + Duration::from_millis(i * 50),
                )
            })
            .collect();
        let posted = decisions
            .iter()
            .filter(|&&d| d == NotifyDecision::MarkAndPost)
            .count();
        assert_eq!(posted, 10);
        // L09 / D7: the two refused still mark.
        assert_eq!(
            decisions[10..],
            [NotifyDecision::Mark, NotifyDecision::Mark]
        );
    }

    /// L10: a finish that does not qualify never uses the budget.
    #[test]
    fn finishes_that_do_not_qualify_do_not_use_the_budget() {
        let start = Instant::now();
        let mut limiter = NotifyLimiter::new();
        let on = cfg(ON, 10);
        for focused_or_short in [
            facts(Duration::from_secs(60), true, true),
            facts(Duration::from_secs(1), true, false),
        ] {
            for _ in 0..100 {
                assert_eq!(
                    decide(&focused_or_short, &on, &mut limiter, start),
                    NotifyDecision::Nothing
                );
            }
        }
        let off = cfg(OFF, 10);
        let long = facts(Duration::from_secs(60), true, false);
        assert_eq!(
            decide(&long, &off, &mut limiter, start),
            NotifyDecision::Nothing,
            "D6"
        );
        assert_eq!(limiter.stored(), 0);
        assert_eq!(
            decide(&long, &on, &mut limiter, start),
            NotifyDecision::MarkAndPost
        );
    }

    /// L11 and the FFI's `uint8_t` values.
    #[test]
    fn constants_and_values() {
        assert_eq!(MAX_NOTIFICATIONS_PER_MINUTE, 10);
        assert_eq!(MAX_NOTIFICATION_TEXT_BYTES, 256);
        assert_eq!(NotifyDecision::Nothing as u8, 0);
        assert_eq!(NotifyDecision::Mark as u8, 1);
        assert_eq!(NotifyDecision::MarkAndPost as u8, 2);
    }
}
