//! Minimal, dependency-free date parsing for adapter timestamps.
//!
//! Accepts the three shapes the fixtures/sources actually carry:
//! - WARC-style compact stamps: `YYYYMMDDHHMMSS` (UTC, as used by Common Crawl WAT/WET),
//! - date-only: `YYYY-MM-DD`,
//! - RFC 3339: `YYYY-MM-DDTHH:MM:SS[Z|±HH:MM]`.
//!
//! Unparseable input yields `None` rather than failing the whole page.

/// Parse a source timestamp into Unix epoch seconds (UTC).
pub fn parse_epoch(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Compact WARC stamp: 14 ASCII digits, e.g. 20240102030405.
    if s.len() == 14 && s.bytes().all(|b| b.is_ascii_digit()) {
        let y: i64 = s[0..4].parse().ok()?;
        let mo: i64 = s[4..6].parse().ok()?;
        let d: i64 = s[6..8].parse().ok()?;
        let h: i64 = s[8..10].parse().ok()?;
        let mi: i64 = s[10..12].parse().ok()?;
        let se: i64 = s[12..14].parse().ok()?;
        return Some(civil_to_epoch(y, mo, d, h, mi, se));
    }

    let (date, rest) = match s.split_once('T') {
        Some((d, r)) => (d, Some(r)),
        None => (s, None),
    };
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let mo: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }

    let mut h = 0i64;
    let mut mi = 0i64;
    let mut se = 0i64;
    let mut offset = 0i64;
    if let Some(rest) = rest {
        let rest = rest
            .strip_suffix('Z')
            .or_else(|| rest.strip_suffix('z'))
            .unwrap_or(rest);
        if !rest.is_empty() {
            let (time, off) = split_offset(rest);
            let mut tp = time.split(':');
            h = tp.next().unwrap_or("0").parse().ok()?;
            mi = tp.next().unwrap_or("0").parse().ok()?;
            se = tp.next().unwrap_or("0").parse().ok()?;
            offset = off;
        }
    }
    Some(civil_to_epoch(y, mo, d, h, mi, se) - offset)
}

/// Split a `HH:MM:SS±HH:MM` time from its trailing numeric offset (`+`/`-`).
fn split_offset(t: &str) -> (&str, i64) {
    if let Some(i) = t.rfind(['+', '-']) {
        if i > 0 {
            let sign = if t.as_bytes()[i] == b'-' { -1 } else { 1 };
            let off = &t[i + 1..];
            let mut p = off.split(':');
            let oh: i64 = p.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            let om: i64 = p.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            return (&t[..i], sign * (oh * 3600 + om * 60));
        }
    }
    (t, 0)
}

fn civil_to_epoch(y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    days_from_civil(y, m, d) * 86400 + h * 3600 + mi * 60 + s
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_zero() {
        assert_eq!(parse_epoch("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn rfc3339_utc() {
        // 2025-06-10T09:00:00Z
        assert_eq!(parse_epoch("2025-06-10T09:00:00Z"), Some(1_749_546_000));
    }

    #[test]
    fn rfc3339_with_offset_is_normalised() {
        // 2025-06-11T18:30:00+02:00 == 2025-06-11T16:30:00Z
        assert_eq!(parse_epoch("2025-06-11T18:30:00+02:00"), Some(1_749_659_400));
    }

    #[test]
    fn date_only_and_warc_stamp() {
        assert_eq!(parse_epoch("2024-01-01"), Some(1_704_067_200));
        assert_eq!(parse_epoch("20240102030405"), parse_epoch("2024-01-02T03:04:05Z"));
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(parse_epoch("not-a-date"), None);
        assert_eq!(parse_epoch(""), None);
    }
}
