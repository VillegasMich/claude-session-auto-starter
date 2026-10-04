//! `ACTIVE_HOURS`: the daily range, in `TIMEZONE`, in which new windows may be started.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Days, LocalResult, NaiveDateTime, NaiveTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;

/// `HH:MM-HH:MM`. The end is exclusive; a range may cross midnight (`22:00-06:00`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveHours {
    pub start: NaiveTime,
    pub end: NaiveTime,
}

impl FromStr for ActiveHours {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const FORMAT: &str = "expected HH:MM-HH:MM, e.g. 07:00-23:00";
        let (start, end) = s.split_once('-').ok_or(FORMAT)?;
        let parse = |t: &str| NaiveTime::parse_from_str(t.trim(), "%H:%M").map_err(|_| FORMAT);
        let (start, end) = (parse(start)?, parse(end)?);
        if start == end {
            return Err("start and end must differ (unset ACTIVE_HOURS for all day)".into());
        }
        Ok(Self { start, end })
    }
}

impl fmt::Display for ActiveHours {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}-{}",
            self.start.format("%H:%M"),
            self.end.format("%H:%M")
        )
    }
}

impl ActiveHours {
    /// Whether `now` falls inside the range, as seen in `tz`.
    pub fn contains(&self, now: DateTime<Utc>, tz: Tz) -> bool {
        let t = now.with_timezone(&tz).time();
        if self.start < self.end {
            self.start <= t && t < self.end
        } else {
            t >= self.start || t < self.end
        }
    }

    /// The first moment strictly after `now` at which the range starts, in UTC.
    pub fn next_start(&self, now: DateTime<Utc>, tz: Tz) -> DateTime<Utc> {
        let today = now.with_timezone(&tz).date_naive();
        (0..=2)
            .filter_map(|d| today.checked_add_days(Days::new(d)))
            .map(|date| resolve(tz, date.and_time(self.start)))
            .find(|t| *t > now)
            // Unreachable in practice: a start time exists on every day within 2 days.
            .unwrap_or(now + TimeDelta::days(1))
    }
}

/// Local wall-clock time to UTC. Ambiguous times (DST fall back) take the earlier instant;
/// times skipped by a DST jump move forward until they exist.
fn resolve(tz: Tz, mut local: NaiveDateTime) -> DateTime<Utc> {
    loop {
        match tz.from_local_datetime(&local) {
            LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => return t.to_utc(),
            LocalResult::None => local += TimeDelta::minutes(15),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono_tz::America::Bogota;
    use chrono_tz::Europe::Madrid;

    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    fn hours(s: &str) -> ActiveHours {
        s.parse().unwrap()
    }

    #[test]
    fn parses_and_displays() {
        assert_eq!(hours("07:00-23:00").to_string(), "07:00-23:00");
        assert_eq!(hours(" 22:30 - 06:15 ").to_string(), "22:30-06:15");
        for bad in [
            "",
            "07:00",
            "7-23",
            "07:00-24:00",
            "25:00-01:00",
            "08:00-08:00",
        ] {
            assert!(bad.parse::<ActiveHours>().is_err(), "{bad}");
        }
    }

    #[test]
    fn contains_same_day_range() {
        let h = hours("07:00-23:00");
        assert!(!h.contains(utc("2026-10-04T06:59:59Z"), Tz::UTC));
        assert!(h.contains(utc("2026-10-04T07:00:00Z"), Tz::UTC));
        assert!(h.contains(utc("2026-10-04T22:59:59Z"), Tz::UTC));
        assert!(!h.contains(utc("2026-10-04T23:00:00Z"), Tz::UTC));
    }

    #[test]
    fn contains_range_crossing_midnight() {
        let h = hours("22:00-06:00");
        assert!(h.contains(utc("2026-10-04T23:30:00Z"), Tz::UTC));
        assert!(h.contains(utc("2026-10-04T05:59:00Z"), Tz::UTC));
        assert!(!h.contains(utc("2026-10-04T12:00:00Z"), Tz::UTC));
    }

    #[test]
    fn contains_uses_timezone() {
        // 12:00 UTC is 07:00 in Bogota (UTC-5).
        let h = hours("07:00-23:00");
        assert!(h.contains(utc("2026-10-04T12:00:00Z"), Bogota));
        assert!(!h.contains(utc("2026-10-04T11:59:00Z"), Bogota));
    }

    #[test]
    fn next_start_today_or_tomorrow() {
        let h = hours("07:00-23:00");
        assert_eq!(
            h.next_start(utc("2026-10-04T03:00:00Z"), Bogota),
            utc("2026-10-04T12:00:00Z")
        );
        // 23:30 in Bogota: next start is tomorrow 07:00 local.
        assert_eq!(
            h.next_start(utc("2026-10-05T04:30:00Z"), Bogota),
            utc("2026-10-05T12:00:00Z")
        );
        // Exactly at the start: the next one is a day later.
        assert_eq!(
            h.next_start(utc("2026-10-04T07:00:00Z"), Tz::UTC),
            utc("2026-10-05T07:00:00Z")
        );
    }

    #[test]
    fn next_start_skipped_by_dst_moves_forward() {
        // Madrid springs forward on 2026-03-29: 02:00-03:00 local does not exist.
        let h = hours("02:30-08:00");
        assert_eq!(
            h.next_start(utc("2026-03-28T23:00:00Z"), Madrid),
            utc("2026-03-29T01:00:00Z") // 03:00 CEST
        );
    }
}
