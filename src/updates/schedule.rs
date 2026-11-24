//! The next Unraid auto-update window: a fixed local time, every day or on one weekday, in an IANA
//! time zone (DST aware through `chrono-tz`).
//!
//! Local times that do not exist (the hour skipped when clocks go forward) move to the first valid
//! minute after the gap, like cron runs a skipped job right after the change. Local times that
//! happen twice (clocks go back) use the first one: cron runs a fixed-time job once.

use chrono::{DateTime, Datelike, Days, LocalResult, NaiveDateTime, NaiveTime, TimeDelta, TimeZone, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Frequency {
    Daily,
    Weekly,
}

/// Day of the week of a weekly schedule (`mon` .. `sun` in JSON).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Day {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Day {
    pub fn weekday(self) -> Weekday {
        match self {
            Self::Mon => Weekday::Mon,
            Self::Tue => Weekday::Tue,
            Self::Wed => Weekday::Wed,
            Self::Thu => Weekday::Thu,
            Self::Fri => Weekday::Fri,
            Self::Sat => Weekday::Sat,
            Self::Sun => Weekday::Sun,
        }
    }
}

/// A validated schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    pub frequency: Frequency,
    /// Only used by weekly schedules.
    pub day: Day,
    pub time: NaiveTime,
    pub tz: Tz,
}

/// `HH:MM` (24 h) to a time of day.
pub fn parse_time(raw: &str) -> Option<NaiveTime> {
    let (h, m) = raw.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    NaiveTime::from_hms_opt(h.parse().ok()?, m.parse().ok()?, 0)
}

/// An IANA zone name (`Europe/Madrid`, `America/Mexico_City`, `UTC`).
pub fn parse_tz(raw: &str) -> Option<Tz> {
    raw.trim().parse().ok()
}

/// The first window strictly after `now` (epoch seconds), as epoch seconds.
pub fn next_window(s: &Schedule, now: i64) -> Option<i64> {
    let today = DateTime::from_timestamp(now, 0)?.with_timezone(&s.tz).date_naive();
    // Start a day early: near midnight UTC the zone's date can lag behind.
    let first = today.checked_sub_days(Days::new(1))?;
    for offset in 0..=9 {
        let day = first.checked_add_days(Days::new(offset))?;
        if s.frequency == Frequency::Weekly && day.weekday() != s.day.weekday() {
            continue;
        }
        let at = resolve(s.tz, day.and_time(s.time))?;
        if at > now {
            return Some(at);
        }
    }
    None
}

/// A local wall-clock time in `tz` to epoch seconds (see the module docs for gaps and overlaps).
fn resolve(tz: Tz, local: NaiveDateTime) -> Option<i64> {
    // Real gaps are at most a few hours long.
    for step in 0..=6 * 60 {
        let t = local.checked_add_signed(TimeDelta::minutes(step))?;
        match tz.from_local_datetime(&t) {
            LocalResult::Single(d) => return Some(d.timestamp()),
            LocalResult::Ambiguous(first, _) => return Some(first.timestamp()),
            LocalResult::None => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> i64 {
        DateTime::parse_from_rfc3339(s).unwrap().timestamp()
    }

    fn sched(frequency: Frequency, day: Day, time: &str, tz: &str) -> Schedule {
        Schedule {
            frequency,
            day,
            time: parse_time(time).unwrap(),
            tz: parse_tz(tz).unwrap(),
        }
    }

    #[test]
    fn parses_times_and_zones() {
        assert_eq!(parse_time("04:00"), NaiveTime::from_hms_opt(4, 0, 0));
        assert_eq!(parse_time("4:05"), NaiveTime::from_hms_opt(4, 5, 0));
        assert_eq!(parse_time("23:59"), NaiveTime::from_hms_opt(23, 59, 0));
        for bad in ["24:00", "12:60", "12", "1:2", "aa:bb", "", "-1:00", "12:00:00", "+1:00"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
        assert!(parse_tz("America/Mexico_City").is_some());
        assert!(parse_tz("UTC").is_some());
        assert!(parse_tz("Mars/Olympus").is_none());
        assert!(parse_tz("").is_none());
    }

    #[test]
    fn daily_window_is_strictly_after_now() {
        let s = sched(Frequency::Daily, Day::Mon, "04:00", "UTC");
        assert_eq!(
            next_window(&s, utc("2026-11-24T03:59:59Z")),
            Some(utc("2026-11-24T04:00:00Z"))
        );
        assert_eq!(
            next_window(&s, utc("2026-11-24T04:00:00Z")),
            Some(utc("2026-11-25T04:00:00Z")),
            "a window equal to now is already taken"
        );
        assert_eq!(
            next_window(&s, utc("2026-11-24T12:00:00Z")),
            Some(utc("2026-11-25T04:00:00Z"))
        );
        // Month and year ends.
        assert_eq!(
            next_window(&s, utc("2026-12-31T05:00:00Z")),
            Some(utc("2027-01-01T04:00:00Z"))
        );
    }

    #[test]
    fn daily_window_in_a_zone_ahead_and_behind_utc() {
        // Mexico City is UTC-6 all year: 04:00 local = 10:00 UTC.
        let s = sched(Frequency::Daily, Day::Mon, "04:00", "America/Mexico_City");
        assert_eq!(
            next_window(&s, utc("2026-11-24T09:00:00Z")),
            Some(utc("2026-11-24T10:00:00Z"))
        );
        // 02:00 UTC is still the previous local day (20:00 on the 23rd).
        assert_eq!(
            next_window(&s, utc("2026-11-24T02:00:00Z")),
            Some(utc("2026-11-24T10:00:00Z"))
        );
        // Tokyo is UTC+9: 04:00 local on the 25th = 19:00 UTC on the 24th.
        let s = sched(Frequency::Daily, Day::Mon, "04:00", "Asia/Tokyo");
        assert_eq!(
            next_window(&s, utc("2026-11-24T18:00:00Z")),
            Some(utc("2026-11-24T19:00:00Z"))
        );
        assert_eq!(
            next_window(&s, utc("2026-11-24T19:00:00Z")),
            Some(utc("2026-11-25T19:00:00Z"))
        );
    }

    #[test]
    fn weekly_window_picks_the_weekday() {
        // 2026-11-24 is a Tuesday.
        let s = sched(Frequency::Weekly, Day::Sun, "03:30", "Europe/Madrid");
        // Madrid is UTC+1 in winter: Sunday 29th 03:30 local = 02:30 UTC.
        assert_eq!(
            next_window(&s, utc("2026-11-24T12:00:00Z")),
            Some(utc("2026-11-29T02:30:00Z"))
        );
        assert_eq!(
            next_window(&s, utc("2026-11-29T02:30:00Z")),
            Some(utc("2026-12-06T02:30:00Z"))
        );
        let tue = sched(Frequency::Weekly, Day::Tue, "13:00", "UTC");
        assert_eq!(
            next_window(&tue, utc("2026-11-24T12:00:00Z")),
            Some(utc("2026-11-24T13:00:00Z"))
        );
        assert_eq!(
            next_window(&tue, utc("2026-11-24T13:00:01Z")),
            Some(utc("2026-12-01T13:00:00Z"))
        );
    }

    #[test]
    fn dst_spring_forward_gap_runs_right_after_the_change() {
        // New York, 2026-03-08: 02:00 EST jumps to 03:00 EDT, so 02:30 does not exist.
        let s = sched(Frequency::Daily, Day::Mon, "02:30", "America/New_York");
        assert_eq!(
            next_window(&s, utc("2026-03-08T05:00:00Z")),
            Some(utc("2026-03-08T07:00:00Z")),
            "03:00 EDT"
        );
        // The next day is normal again: 02:30 EDT = 06:30 UTC.
        assert_eq!(
            next_window(&s, utc("2026-03-08T07:00:00Z")),
            Some(utc("2026-03-09T06:30:00Z"))
        );
        // A time outside the gap just changes offset: 04:00 EST the day before, 04:00 EDT after.
        let s = sched(Frequency::Daily, Day::Mon, "04:00", "America/New_York");
        assert_eq!(
            next_window(&s, utc("2026-03-07T10:00:00Z")),
            Some(utc("2026-03-08T08:00:00Z"))
        );
    }

    #[test]
    fn dst_fall_back_overlap_uses_the_first_time() {
        // New York, 2026-11-01: 02:00 EDT goes back to 01:00 EST, so 01:30 happens twice.
        let s = sched(Frequency::Daily, Day::Mon, "01:30", "America/New_York");
        assert_eq!(
            next_window(&s, utc("2026-11-01T04:00:00Z")),
            Some(utc("2026-11-01T05:30:00Z")),
            "01:30 EDT"
        );
        // Between the two 01:30s: the job already ran, so the next one is tomorrow (01:30 EST).
        assert_eq!(
            next_window(&s, utc("2026-11-01T06:00:00Z")),
            Some(utc("2026-11-02T06:30:00Z"))
        );
        // Madrid, 2026-10-25 (clocks back at 03:00 CEST): a weekly Sunday 04:00 is 03:00 UTC.
        let s = sched(Frequency::Weekly, Day::Sun, "04:00", "Europe/Madrid");
        assert_eq!(
            next_window(&s, utc("2026-10-20T00:00:00Z")),
            Some(utc("2026-10-25T03:00:00Z"))
        );
        // And the Sunday before, still in summer time, was 02:00 UTC.
        assert_eq!(
            next_window(&s, utc("2026-10-13T00:00:00Z")),
            Some(utc("2026-10-18T02:00:00Z"))
        );
    }
}
