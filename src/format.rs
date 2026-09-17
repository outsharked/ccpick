use chrono::{DateTime, Datelike, Local, TimeZone};
use std::path::Path;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Human-friendly "when" for a list row, by local calendar day: `6:30 AM`, `Yesterday 12:00 AM`,
/// `Last Tuesday`, `Last week`, `Earlier this month (9/2)`, `Last month (8/22)`, `7/14`,
/// `12/22/25`.
pub fn friendly(now_ms: i64, ts_ms: Option<i64>) -> String {
    match (to_local(Some(now_ms)), to_local(ts_ms)) {
        (Some(now), Some(ts)) => friendly_in(&now, &ts),
        _ => "-".into(),
    }
}

/// Full local date and time, e.g. `Wed Sep 16, 2026 6:30 AM`.
pub fn exact(ts_ms: Option<i64>) -> String {
    to_local(ts_ms).map_or_else(|| "-".into(), |ts| exact_in(&ts))
}

fn to_local(ts_ms: Option<i64>) -> Option<DateTime<Local>> {
    ts_ms
        .and_then(DateTime::from_timestamp_millis)
        .map(|d| d.with_timezone(&Local))
}

fn friendly_in<Tz: TimeZone>(now: &DateTime<Tz>, ts: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let days = (now.date_naive() - ts.date_naive()).num_days();
    let time = ts.format("%-I:%M %p");
    let month_index = |d: &DateTime<Tz>| d.year() * 12 + d.month0() as i32;
    match days {
        ..=0 => time.to_string(),
        1 => format!("Yesterday {time}"),
        2..=6 => format!("Last {}", ts.format("%A")),
        7..=13 => "Last week".into(),
        _ if month_index(now) == month_index(ts) => {
            format!("Earlier this month ({})", ts.format("%-m/%-d"))
        }
        _ if month_index(now) - month_index(ts) == 1 => {
            format!("Last month ({})", ts.format("%-m/%-d"))
        }
        _ if now.year() == ts.year() => ts.format("%-m/%-d").to_string(),
        _ => ts.format("%-m/%-d/%y").to_string(),
    }
}

fn exact_in<Tz: TimeZone>(ts: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    ts.format("%a %b %-d, %Y %-I:%M %p").to_string()
}

pub fn shorten_home(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Like `shorten_home`, but for a path in `env`'s own form: Windows paths compare
/// case-insensitively with `\` separators. An empty `home` leaves the path unchanged.
pub fn shorten_home_in(path: &Path, home: &Path, env: &crate::env::Env) -> String {
    if home.as_os_str().is_empty() {
        return path.display().to_string();
    }
    if !env.is_windows() {
        return shorten_home(path, home);
    }
    let p = path.to_string_lossy().replace('/', "\\");
    let h = home.to_string_lossy().replace('/', "\\");
    let h = h.trim_end_matches('\\');
    if p.len() >= h.len() && p.is_char_boundary(h.len()) && p[..h.len()].eq_ignore_ascii_case(h) {
        let rest = &p[h.len()..];
        if rest.is_empty() {
            return "~".into();
        }
        if rest.starts_with('\\') {
            return format!("~{rest}");
        }
    }
    p
}

pub fn local_time(ts_ms: Option<i64>) -> String {
    ts_ms
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{FixedOffset, TimeZone};

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> chrono::DateTime<FixedOffset> {
        FixedOffset::west_opt(4 * 3600)
            .unwrap()
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .unwrap()
    }

    #[test]
    fn friendly_buckets_by_calendar_day() {
        // Thursday, Sep 17 2026, 9:00 AM.
        let now = at(2026, 9, 17, 9, 0);
        let f = |ts| friendly_in(&now, &ts);
        assert_eq!(f(at(2026, 9, 17, 6, 30)), "6:30 AM");
        assert_eq!(f(at(2026, 9, 17, 0, 5)), "12:05 AM");
        assert_eq!(f(at(2026, 9, 16, 23, 59)), "Yesterday 11:59 PM");
        assert_eq!(f(at(2026, 9, 16, 0, 0)), "Yesterday 12:00 AM");
        assert_eq!(f(at(2026, 9, 15, 13, 0)), "Last Tuesday");
        assert_eq!(f(at(2026, 9, 11, 13, 0)), "Last Friday");
        assert_eq!(f(at(2026, 9, 10, 13, 0)), "Last week");
        assert_eq!(f(at(2026, 9, 4, 13, 0)), "Last week");
        assert_eq!(f(at(2026, 9, 2, 13, 0)), "Earlier this month (9/2)");
        assert_eq!(f(at(2026, 8, 22, 13, 0)), "Last month (8/22)");
        assert_eq!(f(at(2026, 7, 14, 13, 0)), "7/14");
        assert_eq!(f(at(2025, 12, 22, 13, 0)), "12/22/25");
        // Clock skew: a slightly future timestamp is still "today".
        assert_eq!(f(at(2026, 9, 17, 9, 5)), "9:05 AM");
    }

    #[test]
    fn last_week_takes_precedence_over_month_boundaries() {
        // Wednesday, Sep 2: Aug 25 is 8 days back, in the previous month.
        let now = at(2026, 9, 2, 12, 0);
        assert_eq!(friendly_in(&now, &at(2026, 8, 25, 12, 0)), "Last week");
        assert_eq!(
            friendly_in(&now, &at(2026, 8, 12, 12, 0)),
            "Last month (8/12)"
        );
    }

    #[test]
    fn last_month_wraps_across_the_year() {
        let now = at(2026, 1, 20, 12, 0);
        assert_eq!(
            friendly_in(&now, &at(2025, 12, 1, 12, 0)),
            "Last month (12/1)"
        );
        assert_eq!(friendly_in(&now, &at(2025, 11, 1, 12, 0)), "11/1/25");
    }

    #[test]
    fn exact_formats_full_date_and_time() {
        assert_eq!(
            exact_in(&at(2026, 9, 16, 6, 30)),
            "Wed Sep 16, 2026 6:30 AM"
        );
        assert_eq!(
            exact_in(&at(2026, 9, 16, 18, 5)),
            "Wed Sep 16, 2026 6:05 PM"
        );
    }

    #[test]
    fn missing_timestamps_show_a_dash() {
        assert_eq!(friendly(0, None), "-");
        assert_eq!(exact(None), "-");
    }

    #[test]
    fn shortens_home() {
        let home = Path::new("/home/u");
        assert_eq!(shorten_home(Path::new("/home/u"), home), "~");
        assert_eq!(shorten_home(Path::new("/home/u/code/x"), home), "~/code/x");
        assert_eq!(shorten_home(Path::new("/srv/x"), home), "/srv/x");
    }

    #[test]
    fn shortens_windows_paths_case_insensitively() {
        use crate::env::Env;
        let home = Path::new(r"C:\Users\me");
        assert_eq!(
            shorten_home_in(Path::new(r"c:\users\me\code\x"), home, &Env::Windows),
            r"~\code\x"
        );
        assert_eq!(
            shorten_home_in(Path::new(r"C:\Users\me"), home, &Env::Windows),
            "~"
        );
        assert_eq!(
            shorten_home_in(Path::new(r"C:\Users\meow"), home, &Env::Windows),
            r"C:\Users\meow"
        );
        assert_eq!(
            shorten_home_in(Path::new("/home/u/x"), Path::new("/home/u"), &Env::Linux),
            "~/x"
        );
        assert_eq!(
            shorten_home_in(Path::new("/x"), Path::new(""), &Env::Linux),
            "/x"
        );
    }
}
