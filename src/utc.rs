//! Minimal UTC time helpers.
//!
//! Timestamps are persisted as RFC 3339 UTC strings (`YYYY-MM-DDTHH:MM:SSZ`).
//! Hand-rolled to keep the dependency footprint small; the civil-date math is
//! Howard Hinnant's `civil_from_days` / `days_from_civil`, covered by tests
//! against known epoch values.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_rfc3339() -> String {
    format_epoch(now_epoch())
}

pub fn format_epoch(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

pub fn parse_rfc3339_utc(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    if bytes.len() != 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' || bytes[19] != b'Z' {
        return None;
    }
    if !bytes.iter().enumerate().all(|(i, b)| match i {
        4 | 7 | 10 | 13 | 16 | 19 => true,
        _ => b.is_ascii_digit(),
    }) {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<u64>().ok();
    let y = num(0, 4)? as i64;
    let mo = num(5, 7)? as i64;
    let d = num(8, 10)? as i64;
    let secs_of_day = num(11, 13)? * 3600 + num(14, 16)? * 60 + num(17, 19)?;
    let days = days_from_civil(y, mo, d);
    u64::try_from(days * 86_400).ok()?.checked_add(secs_of_day)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Renders a duration in seconds as a compact human string ("45s", "1h 14m",
/// "3d 4h"). Used for "resets in ..." and sync-freshness output.
pub fn humanize_duration_secs(total_secs: u64) -> String {
    if total_secs < 60 {
        return format!("{total_secs}s");
    }
    let mins = total_secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        let rem = mins % 60;
        return if rem == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {rem}m")
        };
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if rem_hours == 0 {
        format!("{days}d")
    } else {
        format!("{days}d {rem_hours}h")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_epochs() {
        assert_eq!(format_epoch(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_epoch(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(format_epoch(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_epoch(2_000_000_000), "2033-05-18T03:33:20Z");
    }

    #[test]
    fn parses_and_round_trips() {
        for epoch in [0u64, 86_399, 951_782_400, 1_700_913_600, 4_102_444_800] {
            let text = format_epoch(epoch);
            assert_eq!(parse_rfc3339_utc(&text), Some(epoch), "round trip {text}");
        }
        assert_eq!(format_epoch(1_700_913_600), "2023-11-25T12:00:00Z");
    }

    #[test]
    fn rejects_malformed_timestamps() {
        for bad in [
            "",
            "not-a-timestamp",
            "2023-11-25 12:00:00",
            "2023-11-25T12:00:00",
            "2023-13-01T00:00:00X",
            "2023-11-25T12:00:0Z",
        ] {
            assert_eq!(parse_rfc3339_utc(bad), None, "should reject {bad}");
        }
    }

    #[test]
    fn humanizes_durations() {
        assert_eq!(humanize_duration_secs(45), "45s");
        assert_eq!(humanize_duration_secs(120), "2m");
        assert_eq!(humanize_duration_secs(3600), "1h");
        assert_eq!(humanize_duration_secs(3600 + 14 * 60), "1h 14m");
        assert_eq!(humanize_duration_secs(3 * 86_400), "3d");
        assert_eq!(humanize_duration_secs(3 * 86_400 + 4 * 3600), "3d 4h");
    }
}
