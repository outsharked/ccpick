use std::path::Path;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn relative(now_ms: i64, ts_ms: Option<i64>) -> String {
    let Some(ts) = ts_ms else { return "-".into() };
    let secs = ((now_ms - ts) / 1000).max(0);
    const DAY: i64 = 86_400;
    match secs {
        s if s < 60 => "now".into(),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < DAY => format!("{}h", s / 3_600),
        s if s < 30 * DAY => format!("{}d", s / DAY),
        s if s < 365 * DAY => format!("{}mo", s / (30 * DAY)),
        s => format!("{}y", s / (365 * DAY)),
    }
}

pub fn shorten_home(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
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

    #[test]
    fn relative_units() {
        let now = 10_000_000_000;
        assert_eq!(relative(now, None), "-");
        assert_eq!(relative(now, Some(now - 30_000)), "now");
        assert_eq!(relative(now, Some(now - 5 * 60_000)), "5m");
        assert_eq!(relative(now, Some(now - 3 * 3_600_000)), "3h");
        assert_eq!(relative(now, Some(now - 2 * 86_400_000)), "2d");
        assert_eq!(relative(now, Some(now - 65 * 86_400_000)), "2mo");
        assert_eq!(relative(now, Some(now - 800 * 86_400_000)), "2y");
        assert_eq!(relative(now, Some(now + 60_000)), "now");
    }

    #[test]
    fn shortens_home() {
        let home = Path::new("/home/u");
        assert_eq!(shorten_home(Path::new("/home/u"), home), "~");
        assert_eq!(shorten_home(Path::new("/home/u/code/x"), home), "~/code/x");
        assert_eq!(shorten_home(Path::new("/srv/x"), home), "/srv/x");
    }
}
