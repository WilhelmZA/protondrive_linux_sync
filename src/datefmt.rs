//! Tiny date helpers (no chrono dependency) for the ISO-8601 timestamps the
//! proton-drive CLI emits (`claimedModificationTime` etc.) and for trashinfo.
//! Everything is treated as UTC; Proton returns "...Z" times.

/// Days since the Unix epoch for a civil (proleptic Gregorian) date.
/// Howard Hinnant's algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of `days_from_civil`: (year, month, day) from days since epoch.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse an ISO-8601 timestamp (UTC assumed) to epoch seconds. Fractional
/// seconds and any timezone suffix are ignored. Returns None on malformed input.
pub fn iso8601_to_epoch(s: &str) -> Option<i64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    // Expect YYYY-MM-DD[T ]HH:MM:SS
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    let sep = bytes[10];
    if sep != b'T' && sep != b' ' {
        return None;
    }
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let min: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    let days = days_from_civil(year, month, day);
    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

/// Format epoch seconds as "YYYY-MM-DDTHH:MM:SS" (UTC).
pub fn epoch_to_iso(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}")
}

/// Format epoch seconds as "YYYY-MM-DD HH:MM:SSZ" (UTC), for log lines. The
/// trailing Z is explicit because the log is read by a human who is probably
/// not on UTC: without it, a timestamp two hours off the wall clock looks like
/// a bug in the app rather than a timezone.
pub fn epoch_to_log_stamp(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

/// Format epoch seconds as "YYYYMMDD-HHMMSS" (UTC), for conflict-copy names.
pub fn epoch_to_stamp(secs: i64) -> String {
    local_epoch_parts(secs, |y, m, d, h, mi, s| {
        format!("{y:04}{m:02}{d:02}-{h:02}{mi:02}{s:02}")
    })
}

/// Format epoch seconds as "YYYY-MM-DD HH:MM:SS" in the local timezone (no suffix).
pub fn epoch_to_local_log_stamp(secs: i64) -> String {
    local_epoch_parts(secs, |y, m, d, h, mi, s| {
        format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
    })
}

fn local_epoch_parts(
    secs: i64,
    fmt: impl FnOnce(i64, i64, i64, i64, i64, i64) -> String,
) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs;
    let ptr = unsafe { libc::localtime_r(&t, &mut tm) };
    if ptr.is_null() {
        let days = secs.div_euclid(86400);
        let rem = secs.rem_euclid(86400);
        let (y, m, d) = civil_from_days(days);
        let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        return fmt(y, m, d, h, mi, s);
    }
    fmt(
        tm.tm_year as i64 + 1900,
        tm.tm_mon as i64 + 1,
        tm.tm_mday as i64,
        tm.tm_hour as i64,
        tm.tm_min as i64,
        tm.tm_sec as i64,
    )
}

/// "just now", "5 minutes ago", "yesterday": how long before `now` (both in
/// epoch seconds) `ts` was, in the words the window and the tray status use.
pub fn relative_time(now: i64, ts: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=44 => "just now".into(),
        45..=119 => "1 minute ago".into(),
        120..=3599 => format!("{} minutes ago", d / 60),
        3600..=7199 => "1 hour ago".into(),
        7200..=86399 => format!("{} hours ago", d / 3600),
        86400..=172799 => "yesterday".into(),
        _ => format!("{} days ago", d / 86400),
    }
}

/// Current time in epoch seconds (0 on the impossible clock error).
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        // 2026-07-22T12:31:42Z
        let e = iso8601_to_epoch("2026-07-22T12:31:42.992Z").unwrap();
        assert_eq!(epoch_to_iso(e), "2026-07-22T12:31:42");
    }

    #[test]
    fn known_epoch() {
        assert_eq!(iso8601_to_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso8601_to_epoch("2000-01-01T00:00:00Z"), Some(946684800));
    }

    #[test]
    fn local_stamp_respects_tz() {
        use std::ffi::CString;
        extern "C" {
            fn tzset();
        }
        let key = CString::new("TZ").unwrap();
        let val = CString::new("Pacific/Auckland").unwrap();
        unsafe {
            libc::setenv(key.as_ptr(), val.as_ptr(), 1);
            tzset();
        }
        // 2026-01-01 00:00:00 UTC is 2026-01-01 13:00:00 in NZDT (+13).
        let stamp = epoch_to_stamp(1_767_225_600);
        assert_eq!(stamp, "20260101-130000");
        unsafe {
            libc::unsetenv(key.as_ptr());
            tzset();
        }
    }

    #[test]
    fn relative_time_words() {
        assert_eq!(relative_time(1000, 1000), "just now");
        assert_eq!(relative_time(1000, 1100), "just now");
        assert_eq!(relative_time(1000, 1000 - 44), "just now");
        assert_eq!(relative_time(1000, 1000 - 45), "1 minute ago");
        assert_eq!(relative_time(1000, 1000 - 300), "5 minutes ago");
        assert_eq!(relative_time(10_000, 10_000 - 3600), "1 hour ago");
        assert_eq!(relative_time(100_000, 100_000 - 7200), "2 hours ago");
        assert_eq!(relative_time(200_000, 200_000 - 86400), "yesterday");
        assert_eq!(relative_time(400_000, 400_000 - 3 * 86400), "3 days ago");
    }
}
