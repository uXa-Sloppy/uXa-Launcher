//! When the agent may check GitHub, and when it may restart the game server.
//!
//! Checks and downloads can happen on the interval. The dedicated server is
//! only stopped inside the daily window (default 03:00 America/New_York).

use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

pub const DEFAULT_RESTART_TIME: &str = "03:00";
pub const DEFAULT_TIME_ZONE: &str = "America/New_York";

pub fn parse_clock(text: &str) -> Option<NaiveTime> {
    let text = text.trim();
    let (hour, minute) = text.split_once(':')?;
    if hour.is_empty() || minute.is_empty() || minute.len() > 2 {
        return None;
    }
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    NaiveTime::from_hms_opt(hour, minute, 0)
}

pub fn parse_zone(name: &str) -> Result<Tz, String> {
    let name = name.trim();
    // Abbreviations such as EST are a fixed offset and do not follow daylight saving.
    if !name.contains('/') {
        return Err(format!(
            "unknown time zone \"{name}\". Use an IANA name such as America/New_York."
        ));
    }
    name.parse::<Tz>().map_err(|_| {
        format!("unknown time zone \"{name}\". Use an IANA name such as America/New_York.")
    })
}

/// Local civil time, DST-aware. A spring-forward gap moves to the next valid minute.
pub fn in_zone(date: NaiveDate, time: NaiveTime, tz: Tz) -> DateTime<Utc> {
    let naive = date.and_time(time);
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(local) => local.with_timezone(&Utc),
        LocalResult::Ambiguous(early, _) => early.with_timezone(&Utc),
        LocalResult::None => {
            let mut probe = naive;
            for _ in 0..180 {
                probe += Duration::minutes(1);
                if let LocalResult::Single(local) = tz.from_local_datetime(&probe) {
                    return local.with_timezone(&Utc);
                }
            }
            naive.and_utc()
        }
    }
}

pub fn next_restart(now: DateTime<Utc>, time: NaiveTime, tz: Tz) -> DateTime<Utc> {
    let local_date = now.with_timezone(&tz).date_naive();
    let today = in_zone(local_date, time, tz);
    if now < today {
        today
    } else {
        let tomorrow = local_date.succ_opt().unwrap_or(local_date);
        in_zone(tomorrow, time, tz)
    }
}

/// True once per civil day, from `time` until `time + window`, in `tz`.
pub fn maintenance_due(
    now: DateTime<Utc>,
    time: NaiveTime,
    tz: Tz,
    window: Duration,
    last_local_date: Option<NaiveDate>,
) -> bool {
    let local_date = now.with_timezone(&tz).date_naive();
    if last_local_date == Some(local_date) {
        return false;
    }
    let start = in_zone(local_date, time, tz);
    now >= start && now < start + window
}

pub fn local_date(now: DateTime<Utc>, tz: Tz) -> NaiveDate {
    now.with_timezone(&tz).date_naive()
}

/// Startup check, then every `interval`, plus any clock times after the previous check.
pub fn check_due(
    now: DateTime<Utc>,
    booted: DateTime<Utc>,
    startup_delay: Duration,
    interval: Option<Duration>,
    last_check: Option<DateTime<Utc>>,
    startup_done: bool,
    check_times: &[NaiveTime],
    tz: Tz,
) -> bool {
    if !startup_done && now >= booted + startup_delay {
        return true;
    }
    if let (Some(interval), Some(last)) = (interval, last_check) {
        if interval > Duration::zero() && now >= last + interval {
            return true;
        }
    }
    let Some(last) = last_check else {
        return false;
    };
    let today = now.with_timezone(&tz).date_naive();
    for time in check_times {
        let scheduled = in_zone(today, *time, tz);
        if scheduled <= now && last < scheduled {
            return true;
        }
    }
    false
}

pub fn format_local(instant: DateTime<Utc>, tz: Tz) -> String {
    instant
        .with_timezone(&tz)
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    Maintenance,
    Check,
    Sleep,
}

/// Maintenance wins over a startup check so a process that starts at 03:00
/// does not wait out `checkAfterStartSecs` and miss the window.
pub fn decide_tick(
    now: DateTime<Utc>,
    booted: DateTime<Utc>,
    startup_delay: Duration,
    interval: Option<Duration>,
    last_check: Option<DateTime<Utc>>,
    startup_done: bool,
    check_times: &[NaiveTime],
    restart_time: NaiveTime,
    tz: Tz,
    window: Duration,
    last_maintenance: Option<NaiveDate>,
) -> Tick {
    if maintenance_due(now, restart_time, tz, window, last_maintenance) {
        return Tick::Maintenance;
    }
    if check_due(
        now,
        booted,
        startup_delay,
        interval,
        last_check,
        startup_done,
        check_times,
        tz,
    ) {
        return Tick::Check;
    }
    Tick::Sleep
}

pub fn interval_from_hours(hours: f64) -> Option<Duration> {
    if !hours.is_finite() || hours <= 0.0 {
        return None;
    }
    let secs = (hours * 3600.0).round() as i64;
    if secs <= 0 {
        None
    } else {
        Some(Duration::seconds(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz() -> Tz {
        parse_zone(DEFAULT_TIME_ZONE).unwrap()
    }

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn eastern_three_am_follows_daylight_saving() {
        let zone = tz();
        let three = parse_clock("03:00").unwrap();
        // 15 Jan 2026 03:00 EST is 08:00 UTC.
        let winter = in_zone(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(), three, zone);
        assert_eq!(winter, at(2026, 1, 15, 8, 0));
        // 15 Jul 2026 03:00 EDT is 07:00 UTC.
        let summer = in_zone(NaiveDate::from_ymd_opt(2026, 7, 15).unwrap(), three, zone);
        assert_eq!(summer, at(2026, 7, 15, 7, 0));
        // 8 Mar 2026 is the US spring-forward. 03:00 is the first valid instant, 07:00 UTC.
        let spring = in_zone(NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(), three, zone);
        assert_eq!(spring, at(2026, 3, 8, 7, 0));
        // 1 Nov 2026 falls back at 02:00, so 03:00 is unambiguous EST, 08:00 UTC.
        let fall = in_zone(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), three, zone);
        assert_eq!(fall, at(2026, 11, 1, 8, 0));
    }

    #[test]
    fn maintenance_only_inside_the_window_and_once_per_day() {
        let zone = tz();
        let three = parse_clock("3:00").unwrap();
        let window = Duration::minutes(10);
        let before = at(2026, 1, 15, 7, 59);
        assert!(!maintenance_due(before, three, zone, window, None));
        let start = at(2026, 1, 15, 8, 0);
        assert!(maintenance_due(start, three, zone, window, None));
        let late = at(2026, 1, 15, 8, 9);
        assert!(maintenance_due(late, three, zone, window, None));
        let after = at(2026, 1, 15, 8, 10);
        assert!(!maintenance_due(after, three, zone, window, None));
        let today = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        assert!(!maintenance_due(start, three, zone, window, Some(today)));
        // Summer: 03:00 EDT is 07:00 UTC, still not 08:00.
        let july_eight_utc = at(2026, 7, 15, 8, 0);
        assert!(!maintenance_due(july_eight_utc, three, zone, window, None));
        assert!(maintenance_due(
            at(2026, 7, 15, 7, 0),
            three,
            zone,
            window,
            None
        ));
        let next = next_restart(before, three, zone);
        assert_eq!(next, start);
        let after_window = next_restart(after, three, zone);
        assert_eq!(after_window, at(2026, 1, 16, 8, 0));
    }

    #[test]
    fn checks_on_startup_interval_and_clock_times_not_at_restart() {
        let zone = tz();
        let booted = at(2026, 1, 15, 12, 0);
        let delay = Duration::seconds(30);
        let six_hours = Some(Duration::hours(6));
        assert!(!check_due(
            booted + Duration::seconds(10),
            booted,
            delay,
            six_hours,
            None,
            false,
            &[],
            zone
        ));
        assert!(check_due(
            booted + Duration::seconds(30),
            booted,
            delay,
            six_hours,
            None,
            false,
            &[],
            zone
        ));
        let checked = booted + Duration::seconds(30);
        assert!(!check_due(
            checked + Duration::hours(5),
            booted,
            delay,
            six_hours,
            Some(checked),
            true,
            &[],
            zone
        ));
        assert!(check_due(
            checked + Duration::hours(6),
            booted,
            delay,
            six_hours,
            Some(checked),
            true,
            &[],
            zone
        ));
        // 04:00 Eastern on 15 Jan 2026 is 09:00 UTC. A check time is not a restart.
        let four = parse_clock("04:00").unwrap();
        let before_four = at(2026, 1, 15, 8, 30);
        assert!(!check_due(
            before_four,
            booted,
            delay,
            None,
            Some(at(2026, 1, 15, 5, 0)),
            true,
            &[four],
            zone
        ));
        assert!(check_due(
            at(2026, 1, 15, 9, 0),
            booted,
            delay,
            None,
            Some(at(2026, 1, 15, 5, 0)),
            true,
            &[four],
            zone
        ));
    }

    #[test]
    fn rejects_a_bad_clock_or_zone() {
        assert!(parse_clock("03:00").is_some());
        assert!(parse_clock("3:00").is_some());
        assert!(parse_clock("24:00").is_none());
        assert!(parse_clock("03").is_none());
        assert!(parse_zone("America/New_York").is_ok());
        assert!(parse_zone("EST").is_err());
        assert!(parse_zone("Not/AZone").is_err());
    }

    #[test]
    fn maintenance_is_due_before_the_startup_check() {
        let zone = tz();
        let three = parse_clock("03:00").unwrap();
        let window = Duration::minutes(10);
        let booted = at(2026, 1, 15, 8, 0);
        assert_eq!(
            decide_tick(
                booted,
                booted,
                Duration::seconds(30),
                Some(Duration::hours(6)),
                None,
                false,
                &[],
                three,
                zone,
                window,
                None
            ),
            Tick::Maintenance
        );
        let noon = at(2026, 1, 15, 12, 0);
        assert_eq!(
            decide_tick(
                noon,
                noon,
                Duration::seconds(30),
                Some(Duration::hours(6)),
                None,
                false,
                &[],
                three,
                zone,
                window,
                None
            ),
            Tick::Sleep
        );
        assert_eq!(
            decide_tick(
                noon + Duration::seconds(30),
                noon,
                Duration::seconds(30),
                Some(Duration::hours(6)),
                None,
                false,
                &[],
                three,
                zone,
                window,
                None
            ),
            Tick::Check
        );
        let today = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        assert_eq!(
            decide_tick(
                booted + Duration::minutes(5),
                booted,
                Duration::seconds(30),
                None,
                Some(booted),
                true,
                &[],
                three,
                zone,
                window,
                Some(today)
            ),
            Tick::Sleep
        );
        assert!(interval_from_hours(0.0).is_none());
        assert!(interval_from_hours(-1.0).is_none());
        assert_eq!(interval_from_hours(6.0), Some(Duration::hours(6)));
    }
}
