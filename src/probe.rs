//! Is a 5-hour window active? See `docs/architecture.md#detecting-an-active-window`.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::config::Secret;
use crate::state::StateStore;

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const USAGE_BETA: &str = "oauth-2025-04-20";
const USAGE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowStatus {
    Active {
        resets_at: DateTime<Utc>,
    },
    Inactive,
    /// The probe couldn't tell.
    Unknown,
}

/// Which strategy produced a [`WindowStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    OAuthUsage,
    Local,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OAuthUsage => "oauth-usage",
            Self::Local => "local",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    pub status: WindowStatus,
    pub source: Source,
}

pub trait UsageProbe {
    fn check(&self, now: DateTime<Utc>) -> Probe;
}

/// The subscription usage endpoint that backs Claude Code's `/usage` screen. Free to call; it
/// is subscription data, not billed API usage.
pub struct UsageApiProbe {
    agent: ureq::Agent,
    token: Secret,
    /// Last error, so a persistent failure is logged as a warning once, not every cycle.
    last_error: RefCell<Option<String>>,
    /// Set after a 401/403: the token can't read usage and won't until it is replaced (which
    /// needs a restart), so the endpoint is not called again.
    rejected: Cell<bool>,
}

impl UsageApiProbe {
    pub fn new(token: Secret) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(USAGE_TIMEOUT))
            .http_status_as_error(false)
            .user_agent(concat!(
                "claude-session-starter/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .into();
        Self {
            agent,
            token,
            last_error: RefCell::new(None),
            rejected: Cell::new(false),
        }
    }

    fn fetch(&self) -> Result<String, String> {
        let mut response = self
            .agent
            .get(USAGE_URL)
            .header("Authorization", &format!("Bearer {}", self.token.expose()))
            .header("anthropic-beta", USAGE_BETA)
            .call()
            .map_err(|e| format!("request failed: {e}"))?;
        let status = response.status();
        if status == 401 || status == 403 {
            self.rejected.set(true);
            return Err(format!(
                "HTTP {status}: the token can't read usage (`claude setup-token` tokens lack the \
                 user:profile scope); not calling it again until restart"
            ));
        }
        if status == 429 {
            return Err("HTTP 429: usage endpoint rate limited; try again later".into());
        }
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("reading response: {e}"))
    }

    fn report(&self, result: &Result<WindowStatus, String>) {
        let mut last = self.last_error.borrow_mut();
        match result {
            Ok(_) => *last = None,
            Err(e) if last.as_deref() == Some(e) => debug!(error = %e, "usage endpoint failed"),
            Err(e) => {
                warn!(error = %e, "usage endpoint failed");
                *last = Some(e.clone());
            }
        }
    }
}

impl UsageProbe for UsageApiProbe {
    fn check(&self, now: DateTime<Utc>) -> Probe {
        if self.rejected.get() {
            return Probe {
                status: WindowStatus::Unknown,
                source: Source::OAuthUsage,
            };
        }
        let result = self.fetch().and_then(|body| parse_usage(&body, now));
        self.report(&result);
        Probe {
            status: result.unwrap_or(WindowStatus::Unknown),
            source: Source::OAuthUsage,
        }
    }
}

/// Parses the usage response defensively: only `five_hour.resets_at` matters.
pub fn parse_usage(body: &str, now: DateTime<Utc>) -> Result<WindowStatus, String> {
    #[derive(Deserialize)]
    struct FiveHour {
        resets_at: Option<DateTime<Utc>>,
    }

    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let five_hour = value
        .get("five_hour")
        .ok_or("response has no `five_hour` field; the format may have changed")?;
    if five_hour.is_null() {
        return Ok(WindowStatus::Inactive);
    }
    let five_hour: FiveHour = serde_json::from_value(five_hour.clone())
        .map_err(|e| format!("unexpected `five_hour`: {e}"))?;
    Ok(match five_hour.resets_at {
        Some(resets_at) if resets_at > now => WindowStatus::Active { resets_at },
        _ => WindowStatus::Inactive,
    })
}

/// Windows this service started itself (or learned about from the usage endpoint).
pub struct LocalStateProbe<'a> {
    store: &'a dyn StateStore,
}

impl<'a> LocalStateProbe<'a> {
    pub fn new(store: &'a dyn StateStore) -> Self {
        Self { store }
    }
}

impl UsageProbe for LocalStateProbe<'_> {
    fn check(&self, now: DateTime<Utc>) -> Probe {
        let status = match self.store.load().and_then(|s| s.resets_at) {
            Some(resets_at) if resets_at > now => WindowStatus::Active { resets_at },
            _ => WindowStatus::Inactive,
        };
        Probe {
            status,
            source: Source::Local,
        }
    }
}

/// Tries `primary`; if it can't tell, asks `fallback`.
pub struct AutoProbe<'a> {
    primary: Box<dyn UsageProbe + 'a>,
    fallback: Box<dyn UsageProbe + 'a>,
}

impl<'a> AutoProbe<'a> {
    pub fn new(primary: Box<dyn UsageProbe + 'a>, fallback: Box<dyn UsageProbe + 'a>) -> Self {
        Self { primary, fallback }
    }
}

impl UsageProbe for AutoProbe<'_> {
    fn check(&self, now: DateTime<Utc>) -> Probe {
        match self.primary.check(now) {
            Probe {
                status: WindowStatus::Unknown,
                ..
            } => self.fallback.check(now),
            probe => probe,
        }
    }
}

#[cfg(test)]
pub mod testing {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    /// Returns queued answers in order; when empty, keeps returning the last one.
    pub struct FakeProbe {
        answers: RefCell<VecDeque<WindowStatus>>,
        source: Source,
        pub calls: std::cell::Cell<usize>,
    }

    impl FakeProbe {
        pub fn new(source: Source, answers: impl IntoIterator<Item = WindowStatus>) -> Self {
            Self {
                answers: RefCell::new(answers.into_iter().collect()),
                source,
                calls: Default::default(),
            }
        }
    }

    impl UsageProbe for FakeProbe {
        fn check(&self, _now: DateTime<Utc>) -> Probe {
            self.calls.set(self.calls.get() + 1);
            let mut answers = self.answers.borrow_mut();
            let status = if answers.len() > 1 {
                answers.pop_front().unwrap()
            } else {
                *answers
                    .front()
                    .expect("FakeProbe needs at least one answer")
            };
            Probe {
                status,
                source: self.source,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeProbe;
    use super::*;
    use crate::state::State;
    use crate::state::testing::MemoryStateStore;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    const NOW: &str = "2026-10-04T15:00:00Z";

    #[test]
    fn usage_with_future_reset_is_active() {
        let body = r#"{"five_hour":{"utilization":4.0,"resets_at":"2026-10-04T19:00:00.110159+00:00",
                       "locked_reason":null},"seven_day":{"utilization":10.0}}"#;
        assert_eq!(
            parse_usage(body, utc(NOW)),
            Ok(WindowStatus::Active {
                resets_at: utc("2026-10-04T19:00:00.110159Z")
            })
        );
    }

    #[test]
    fn usage_without_window_is_inactive() {
        for body in [
            r#"{"five_hour":null}"#,
            r#"{"five_hour":{"utilization":0.0,"resets_at":null}}"#,
            r#"{"five_hour":{"utilization":0.0}}"#,
            r#"{"five_hour":{"resets_at":"2026-10-04T14:59:59Z"}}"#,
        ] {
            assert_eq!(
                parse_usage(body, utc(NOW)),
                Ok(WindowStatus::Inactive),
                "{body}"
            );
        }
    }

    #[test]
    fn unexpected_usage_shapes_are_errors() {
        for body in [
            "",
            "<html>",
            r#"{"seven_day":{}}"#,
            r#"{"five_hour":{"resets_at":"tomorrow"}}"#,
        ] {
            assert!(parse_usage(body, utc(NOW)).is_err(), "{body}");
        }
    }

    #[test]
    fn local_probe_reads_state() {
        let empty = MemoryStateStore::default();
        assert_eq!(
            LocalStateProbe::new(&empty).check(utc(NOW)).status,
            WindowStatus::Inactive
        );

        let store = MemoryStateStore::with(State {
            resets_at: Some(utc("2026-10-04T17:00:00Z")),
            ..State::default()
        });
        let probe = LocalStateProbe::new(&store);
        assert_eq!(
            probe.check(utc(NOW)),
            Probe {
                status: WindowStatus::Active {
                    resets_at: utc("2026-10-04T17:00:00Z")
                },
                source: Source::Local
            }
        );
        assert_eq!(
            probe.check(utc("2026-10-04T17:00:00Z")).status,
            WindowStatus::Inactive
        );
    }

    #[test]
    fn auto_falls_back_only_when_unknown() {
        let active = WindowStatus::Active {
            resets_at: utc("2026-10-04T18:00:00Z"),
        };
        let auto = AutoProbe::new(
            Box::new(FakeProbe::new(Source::OAuthUsage, [WindowStatus::Inactive])),
            Box::new(FakeProbe::new(Source::Local, [active])),
        );
        assert_eq!(auto.check(utc(NOW)).source, Source::OAuthUsage);
        assert_eq!(auto.check(utc(NOW)).status, WindowStatus::Inactive);

        let auto = AutoProbe::new(
            Box::new(FakeProbe::new(Source::OAuthUsage, [WindowStatus::Unknown])),
            Box::new(FakeProbe::new(Source::Local, [active])),
        );
        assert_eq!(
            auto.check(utc(NOW)),
            Probe {
                status: active,
                source: Source::Local
            }
        );
    }
}
