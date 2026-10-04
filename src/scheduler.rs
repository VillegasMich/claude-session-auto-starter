//! Decides what to do next and sleeps until then. See `docs/architecture.md#scheduler`.

use std::time::Duration;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use chrono_tz::Tz;
use tracing::{error, info, warn};

use crate::clock::{Clock, sleep_until};
use crate::hours::ActiveHours;
use crate::probe::{Probe, Source, UsageProbe, WindowStatus};
use crate::retry::Backoff;
use crate::starter::{StartOutcome, Starter};
use crate::state::{State, StateStore};

/// Length of a subscription usage window.
pub const WINDOW: TimeDelta = TimeDelta::hours(5);

/// Extra wait after a window resets, for clock skew between this host and Anthropic.
pub const GRACE: TimeDelta = TimeDelta::seconds(30);

/// `State::source` for a reset time reported by `claude` itself (its `rate_limit_event`).
const FROM_STARTER: &str = "claude";

/// First retry delay after a failed start; doubles up to `CHECK_INTERVAL_MINUTES`.
const FIRST_RETRY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub check_interval: Duration,
    pub active_hours: Option<ActiveHours>,
    pub timezone: Tz,
}

/// What one scheduler cycle found or did, and when the next one should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cycle {
    OutsideActiveHours {
        until: DateTime<Utc>,
    },
    Active {
        resets_at: DateTime<Utc>,
        source: Source,
    },
    Started {
        resets_at: DateTime<Utc>,
    },
    /// `claude` hit a usage limit: a window is active (reset time if `claude` reported it).
    LimitReached {
        resets_at: Option<DateTime<Utc>>,
    },
    StartFailed {
        failures: u32,
        retry_in: Duration,
    },
    /// Every detection strategy failed.
    Unknown {
        retry_in: Duration,
    },
}

pub struct Scheduler<'a> {
    clock: &'a dyn Clock,
    probe: &'a dyn UsageProbe,
    starter: &'a dyn Starter,
    store: &'a dyn StateStore,
    settings: Settings,
    failures: u32,
}

impl<'a> Scheduler<'a> {
    pub fn new(
        clock: &'a dyn Clock,
        probe: &'a dyn UsageProbe,
        starter: &'a dyn Starter,
        store: &'a dyn StateStore,
        settings: Settings,
    ) -> Self {
        Self {
            clock,
            probe,
            starter,
            store,
            settings,
            failures: 0,
        }
    }

    /// Runs cycles until shutdown.
    pub fn run_forever(&mut self) {
        while !self.clock.shutdown_requested() {
            let cycle = self.cycle(true);
            let wake = self.next_wake(&cycle);
            info!(next_check = %fmt_time(wake), "sleeping");
            if !sleep_until(self.clock, wake) {
                break;
            }
        }
        info!("shutdown requested; stopping");
    }

    /// One check: probe, and start a window if none is active. With `respect_hours`, does
    /// nothing outside `ACTIVE_HOURS`.
    pub fn cycle(&mut self, respect_hours: bool) -> Cycle {
        let now = self.clock.now();
        if respect_hours
            && let Some(hours) = self.settings.active_hours
            && !hours.contains(now, self.settings.timezone)
        {
            let until = hours.next_start(now, self.settings.timezone);
            info!(active_hours = %hours, until = %fmt_time(until), "outside active hours");
            return Cycle::OutsideActiveHours { until };
        }

        let probe = self.probe.check(now);
        match probe.status {
            WindowStatus::Active { resets_at } => {
                self.remember(probe);
                info!(source = %probe.source, resets_at = %fmt_time(resets_at), "window active");
                Cycle::Active {
                    resets_at,
                    source: probe.source,
                }
            }
            WindowStatus::Inactive => {
                info!(source = %probe.source, "no active window; starting one");
                self.start()
            }
            WindowStatus::Unknown => {
                let retry_in = self.settings.check_interval;
                warn!(
                    retry_in_secs = retry_in.as_secs(),
                    "cannot tell whether a window is active"
                );
                Cycle::Unknown { retry_in }
            }
        }
    }

    /// Sends the starter message now, without checking first.
    pub fn start(&mut self) -> Cycle {
        let now = self.clock.now();
        match self.starter.start() {
            Ok(StartOutcome::Started(report)) => {
                self.failures = 0;
                let (mut resets_at, source) = match report.resets_at {
                    Some(t) if t > now => (t, FROM_STARTER.to_owned()),
                    _ => (now + WINDOW, Source::Local.to_string()),
                };
                self.save(State {
                    last_start: Some(now),
                    resets_at: Some(resets_at),
                    source: Some(source),
                });
                // Without a reset time from `claude`, ask the usage endpoint for the real one.
                let probe = match report.resets_at {
                    Some(_) => None,
                    None => Some(self.probe.check(self.clock.now())),
                };
                match probe {
                    Some(Probe {
                        status: WindowStatus::Active { resets_at: actual },
                        source: Source::OAuthUsage,
                    }) => {
                        resets_at = actual;
                        self.remember_with_start(actual, Some(now));
                    }
                    Some(Probe {
                        status: WindowStatus::Inactive,
                        source: Source::OAuthUsage,
                    }) => warn!(
                        "starter message sent, but the usage endpoint doesn't report a window \
                         yet; if this persists, the starter model may not open a window \
                         (try STARTER_MODEL=sonnet)"
                    ),
                    _ => {}
                }
                info!(
                    resets_at = %fmt_time(resets_at),
                    duration_ms = report.duration.as_millis() as u64,
                    input_tokens = report.input_tokens,
                    output_tokens = report.output_tokens,
                    cache_read_tokens = report.cache_read_tokens,
                    cache_creation_tokens = report.cache_creation_tokens,
                    reset_source = if report.resets_at.is_some() { "claude" } else { "estimate" },
                    "window started"
                );
                Cycle::Started { resets_at }
            }
            Ok(StartOutcome::LimitReached { message, resets_at }) => {
                self.failures = 0;
                let resets_at = resets_at.filter(|t| *t > now);
                if let Some(t) = resets_at {
                    self.save(State {
                        last_start: self.store.load().and_then(|s| s.last_start),
                        resets_at: Some(t),
                        source: Some(FROM_STARTER.to_owned()),
                    });
                }
                warn!(
                    %message,
                    resets_at = resets_at.map(fmt_time).unwrap_or("unknown".into()),
                    "usage limit reached: a window is already active"
                );
                Cycle::LimitReached { resets_at }
            }
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                let retry_in = Backoff {
                    initial: FIRST_RETRY,
                    max: self.settings.check_interval.max(FIRST_RETRY),
                }
                .delay(self.failures);
                error!(
                    error = format!("{e:#}"),
                    failures = self.failures,
                    retry_in_secs = retry_in.as_secs(),
                    "failed to start a window"
                );
                Cycle::StartFailed {
                    failures: self.failures,
                    retry_in,
                }
            }
        }
    }

    /// When the cycle after `cycle` should run.
    pub fn next_wake(&self, cycle: &Cycle) -> DateTime<Utc> {
        let now = self.clock.now();
        let after = |d: Duration| now + TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX);
        match cycle {
            Cycle::OutsideActiveHours { until } => *until,
            Cycle::Active { resets_at, .. } | Cycle::Started { resets_at } => {
                (*resets_at + GRACE).max(now)
            }
            Cycle::LimitReached {
                resets_at: Some(resets_at),
            } => (*resets_at + GRACE).max(now),
            Cycle::LimitReached { resets_at: None } => after(self.settings.check_interval),
            Cycle::StartFailed { retry_in, .. } | Cycle::Unknown { retry_in } => after(*retry_in),
        }
    }

    /// Records a reset time reported by the usage endpoint, so the `local` fallback knows
    /// about windows started elsewhere too.
    fn remember(&self, probe: Probe) {
        if let (Source::OAuthUsage, WindowStatus::Active { resets_at }) =
            (probe.source, probe.status)
        {
            let last_start = self.store.load().and_then(|s| s.last_start);
            self.remember_with_start(resets_at, last_start);
        }
    }

    fn remember_with_start(&self, resets_at: DateTime<Utc>, last_start: Option<DateTime<Utc>>) {
        let state = State {
            last_start,
            resets_at: Some(resets_at),
            source: Some(Source::OAuthUsage.to_string()),
        };
        if self.store.load().as_ref() != Some(&state) {
            self.save(state);
        }
    }

    fn save(&self, state: State) {
        if let Err(e) = self.store.save(&state) {
            // Not fatal: only the `local` fallback depends on it.
            warn!(error = format!("{e:#}"), "cannot save state");
        }
    }
}

pub fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::testing::FakeClock;
    use crate::probe::testing::FakeProbe;
    use crate::starter::StartReport;
    use crate::starter::testing::FakeStarter;
    use crate::state::testing::MemoryStateStore;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    fn settings() -> Settings {
        Settings {
            check_interval: Duration::from_secs(5 * 60),
            active_hours: None,
            timezone: Tz::UTC,
        }
    }

    const NOW: &str = "2026-10-04T08:00:00Z";

    fn active(at: &str) -> WindowStatus {
        WindowStatus::Active { resets_at: utc(at) }
    }

    #[test]
    fn active_window_sleeps_until_reset_without_starting() {
        let clock = FakeClock::at(NOW);
        let probe = FakeProbe::new(Source::OAuthUsage, [active("2026-10-04T10:00:00Z")]);
        let starter = FakeStarter::new([]);
        let store = MemoryStateStore::default();
        let mut s = Scheduler::new(&clock, &probe, &starter, &store, settings());

        let cycle = s.cycle(true);
        assert_eq!(
            cycle,
            Cycle::Active {
                resets_at: utc("2026-10-04T10:00:00Z"),
                source: Source::OAuthUsage
            }
        );
        assert_eq!(s.next_wake(&cycle), utc("2026-10-04T10:00:30Z"));
        assert_eq!(starter.calls.get(), 0);
        // Remembered for the local fallback.
        assert_eq!(
            store.get().unwrap().resets_at,
            Some(utc("2026-10-04T10:00:00Z"))
        );
    }

    #[test]
    fn inactive_window_is_started_and_reset_time_taken_from_usage() {
        let clock = FakeClock::at(NOW);
        let probe = FakeProbe::new(
            Source::OAuthUsage,
            [WindowStatus::Inactive, active("2026-10-04T13:00:00Z")],
        );
        let starter = FakeStarter::new([FakeStarter::ok()]);
        let store = MemoryStateStore::default();
        let mut s = Scheduler::new(&clock, &probe, &starter, &store, settings());

        let cycle = s.cycle(true);
        assert_eq!(
            cycle,
            Cycle::Started {
                resets_at: utc("2026-10-04T13:00:00Z")
            }
        );
        assert_eq!(starter.calls.get(), 1);
        let state = store.get().unwrap();
        assert_eq!(state.last_start, Some(utc(NOW)));
        assert_eq!(state.resets_at, Some(utc("2026-10-04T13:00:00Z")));
        assert_eq!(state.source.as_deref(), Some("oauth-usage"));
    }

    #[test]
    fn local_start_estimates_five_hours() {
        let clock = FakeClock::at(NOW);
        let store = MemoryStateStore::default();
        let probe = crate::probe::LocalStateProbe::new(&store);
        let starter = FakeStarter::new([FakeStarter::ok()]);
        let mut s = Scheduler::new(&clock, &probe, &starter, &store, settings());

        let cycle = s.cycle(true);
        assert_eq!(
            cycle,
            Cycle::Started {
                resets_at: utc("2026-10-04T13:00:00Z")
            }
        );
        assert_eq!(store.get().unwrap().source.as_deref(), Some("local"));
        // The next probe sees the window this service started.
        assert!(matches!(s.cycle(true), Cycle::Active { .. }));
        assert_eq!(starter.calls.get(), 1);
    }

    #[test]
    fn reset_time_reported_by_claude_wins_without_probing() {
        let clock = FakeClock::at(NOW);
        // 403 for a setup-token: the post-start probe must not be needed.
        let probe = FakeProbe::new(Source::Local, [WindowStatus::Inactive]);
        let starter = FakeStarter::new([Ok(StartOutcome::Started(StartReport {
            resets_at: Some(utc("2026-10-04T12:00:00Z")),
            ..StartReport::default()
        }))]);
        let store = MemoryStateStore::default();
        let mut s = Scheduler::new(&clock, &probe, &starter, &store, settings());

        let cycle = s.cycle(true);
        assert_eq!(
            cycle,
            Cycle::Started {
                resets_at: utc("2026-10-04T12:00:00Z")
            }
        );
        assert_eq!(probe.calls.get(), 1, "only the probe before starting");
        assert_eq!(s.next_wake(&cycle), utc("2026-10-04T12:00:30Z"));
        let state = store.get().unwrap();
        assert_eq!(state.resets_at, Some(utc("2026-10-04T12:00:00Z")));
        assert_eq!(state.source.as_deref(), Some("claude"));
    }

    #[test]
    fn limit_with_reset_time_sleeps_until_reset() {
        let clock = FakeClock::at(NOW);
        let probe = FakeProbe::new(Source::Local, [WindowStatus::Inactive]);
        let starter = FakeStarter::new([Ok(StartOutcome::LimitReached {
            message: "You've hit your limit".into(),
            resets_at: Some(utc("2026-10-04T10:00:00Z")),
        })]);
        let store = MemoryStateStore::default();
        let mut s = Scheduler::new(&clock, &probe, &starter, &store, settings());
        let cycle = s.cycle(true);
        assert_eq!(s.next_wake(&cycle), utc("2026-10-04T10:00:30Z"));
        assert_eq!(
            store.get().unwrap().resets_at,
            Some(utc("2026-10-04T10:00:00Z"))
        );
    }

    #[test]
    fn unknown_waits_check_interval() {
        let clock = FakeClock::at(NOW);
        let probe = FakeProbe::new(Source::OAuthUsage, [WindowStatus::Unknown]);
        let starter = FakeStarter::new([]);
        let store = MemoryStateStore::default();
        let mut s = Scheduler::new(&clock, &probe, &starter, &store, settings());

        let cycle = s.cycle(true);
        assert_eq!(s.next_wake(&cycle), utc("2026-10-04T08:05:00Z"));
        assert_eq!(starter.calls.get(), 0);
    }

    #[test]
    fn failed_starts_back_off_up_to_check_interval() {
        let clock = FakeClock::at(NOW);
        let probe = FakeProbe::new(Source::Local, [WindowStatus::Inactive]);
        let starter = FakeStarter::new((0..5).map(|_| Err("network down".to_owned())));
        let store = MemoryStateStore::default();
        let mut s = Scheduler::new(&clock, &probe, &starter, &store, settings());

        let retries: Vec<u64> = (0..5)
            .map(|_| match s.cycle(true) {
                Cycle::StartFailed { retry_in, .. } => retry_in.as_secs() / 60,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(retries, [1, 2, 4, 5, 5]);
    }

    #[test]
    fn success_resets_backoff() {
        let clock = FakeClock::at(NOW);
        let probe = FakeProbe::new(Source::Local, [WindowStatus::Inactive]);
        let starter = FakeStarter::new([Err("down".into()), FakeStarter::ok(), Err("down".into())]);
        let store = MemoryStateStore::default();
        let mut s = Scheduler::new(&clock, &probe, &starter, &store, settings());
        s.cycle(true);
        s.cycle(true);
        assert_eq!(
            s.cycle(true),
            Cycle::StartFailed {
                failures: 1,
                retry_in: FIRST_RETRY
            }
        );
    }

    #[test]
    fn limit_reached_counts_as_active() {
        let clock = FakeClock::at(NOW);
        let probe = FakeProbe::new(Source::Local, [WindowStatus::Inactive]);
        let starter = FakeStarter::new([Ok(StartOutcome::LimitReached {
            message: "You've hit your limit".into(),
            resets_at: None,
        })]);
        let store = MemoryStateStore::default();
        let mut s = Scheduler::new(&clock, &probe, &starter, &store, settings());
        let cycle = s.cycle(true);
        assert_eq!(cycle, Cycle::LimitReached { resets_at: None });
        assert_eq!(s.next_wake(&cycle), utc("2026-10-04T08:05:00Z"));
    }

    #[test]
    fn outside_active_hours_does_nothing() {
        let clock = FakeClock::at("2026-10-04T05:00:00Z");
        let probe = FakeProbe::new(Source::OAuthUsage, [WindowStatus::Inactive]);
        let starter = FakeStarter::new([]);
        let store = MemoryStateStore::default();
        let settings = Settings {
            active_hours: Some("07:00-23:00".parse().unwrap()),
            ..settings()
        };
        let mut s = Scheduler::new(&clock, &probe, &starter, &store, settings);

        let cycle = s.cycle(true);
        assert_eq!(
            cycle,
            Cycle::OutsideActiveHours {
                until: utc("2026-10-04T07:00:00Z")
            }
        );
        assert_eq!(probe.calls.get(), 0);
        assert_eq!(starter.calls.get(), 0);
        // Manual commands may ignore the range.
        let probe_after = FakeProbe::new(Source::OAuthUsage, [active("2026-10-04T09:00:00Z")]);
        let mut s = Scheduler::new(&clock, &probe_after, &starter, &store, settings);
        assert!(matches!(s.cycle(false), Cycle::Active { .. }));
    }

    #[test]
    fn run_forever_chains_windows() {
        // 5 h 30 s in 60 s chunks is 301 sleeps; the 302nd reports a shutdown.
        let clock = FakeClock::at(NOW).interrupt_after(301);
        let store = MemoryStateStore::default();
        let probe = crate::probe::LocalStateProbe::new(&store);
        let starter = FakeStarter::new([FakeStarter::ok(), FakeStarter::ok()]);
        Scheduler::new(&clock, &probe, &starter, &store, settings()).run_forever();

        assert_eq!(starter.calls.get(), 2);
        assert_eq!(
            store.get().unwrap().last_start,
            Some(utc("2026-10-04T13:00:30Z"))
        );
    }
}
