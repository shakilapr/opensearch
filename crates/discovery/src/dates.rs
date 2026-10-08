//! Small, dependency-free date parsers.
//!
//! Only what feed discovery needs: RFC 822 (RSS `<pubDate>`) and RFC 3339 (Atom
//! `<published>`/`<updated>`) to Unix seconds. No timezone database, no chrono.

/// Parse an RFC 822 / RFC 1123 date to Unix seconds.
///
/// Accepts forms like `Tue, 10 Jun 2025 09:00:00 GMT` and
/// `Wed, 11 Jun 2025 18:30:00 +0200`.
pub fn parse_rfc822(input: &str) -> Option<i64> {
    let input = input.trim();
    // Drop an optional leading weekday ("Tue,").
    let rest = match input.find(',') {
        Some(i) => &input[i + 1..],
        None => input,
    };
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    if tokens.len() < 4 {
        return None;
    }
    let day: i64 = tokens[0].parse().ok()?;
    let month = month_from_abbr(tokens[1])?;
    let year = parse_year(tokens[2])?;
    let (hour, min, sec) = parse_hms(tokens[3])?;
    let tz = tokens.get(4).map(|t| parse_tz(t)).unwrap_or(0);
    if !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day as u32);
    Some(days * 86_400 + hour * 3600 + min * 60 + sec - tz)
}

/// Parse an RFC 3339 timestamp to Unix seconds.
///
/// Accepts `2025-06-10T09:00:00Z`, fractional seconds, and numeric offsets
/// such as `2025-06-11T18:30:00+02:00`.
pub fn parse_rfc3339(input: &str) -> Option<i64> {
    let input = input.trim();
    let b = input.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let year: i64 = input.get(0..4)?.parse().ok()?;
    if b[4] != b'-' {
        return None;
    }
    let month: u32 = input.get(5..7)?.parse().ok()?;
    if b[7] != b'-' {
        return None;
    }
    let day: u32 = input.get(8..10)?.parse().ok()?;
    if b[10] != b'T' && b[10] != b't' && b[10] != b' ' {
        return None;
    }
    let hour: i64 = input.get(11..13)?.parse().ok()?;
    if b[13] != b':' {
        return None;
    }
    let min: i64 = input.get(14..16)?.parse().ok()?;
    if b[16] != b':' {
        return None;
    }
    let sec: i64 = input.get(17..19)?.parse().ok()?;

    let mut idx = 19;
    // Skip fractional seconds.
    if idx < b.len() && b[idx] == b'.' {
        idx += 1;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
    }
    let tz = if idx >= b.len() {
        0
    } else if b[idx] == b'Z' || b[idx] == b'z' {
        0
    } else if b[idx] == b'+' || b[idx] == b'-' {
        let sign: i64 = if b[idx] == b'-' { -1 } else { 1 };
        let digits: String = input[idx + 1..]
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        if digits.len() < 4 {
            return None;
        }
        let hh: i64 = digits.get(0..2)?.parse().ok()?;
        let mm: i64 = digits.get(2..4)?.parse().ok()?;
        sign * (hh * 3600 + mm * 60)
    } else {
        0
    };

    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3600 + min * 60 + sec - tz)
}

fn month_from_abbr(s: &str) -> Option<u32> {
    let s = s.trim();
    let key = s.get(0..3)?.to_ascii_lowercase();
    Some(match key.as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    })
}

/// Two-digit years pivot at 70 (RFC 2822), four-digit years are literal.
fn parse_year(s: &str) -> Option<i64> {
    let n: i64 = s.trim().parse().ok()?;
    Some(if n >= 100 {
        n
    } else if n < 70 {
        2000 + n
    } else {
        1900 + n
    })
}

fn parse_hms(s: &str) -> Option<(i64, i64, i64)> {
    // Strip any trailing zone glued to the time ("09:00:00GMT").
    let end = s
        .find(|c: char| !c.is_ascii_digit() && c != ':')
        .unwrap_or(s.len());
    let mut parts = s[..end].split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let sec: i64 = parts.next().unwrap_or("0").parse().ok()?;
    Some((h, m, sec))
}

/// Offset in seconds east of UTC for an RFC 822 zone token.
fn parse_tz(t: &str) -> i64 {
    let t = t.trim();
    match t {
        "+0000" | "-0000" | "GMT" | "UT" | "UTC" | "Z" => 0,
        "EST" => -5 * 3600,
        "EDT" => -4 * 3600,
        "CST" => -6 * 3600,
        "CDT" => -5 * 3600,
        "MST" => -7 * 3600,
        "MDT" => -6 * 3600,
        "PST" => -8 * 3600,
        "PDT" => -7 * 3600,
        _ => {
            let bytes = t.as_bytes();
            if bytes.first() == Some(&b'+') || bytes.first() == Some(&b'-') {
                let digits: String = t[1..].chars().filter(|c| c.is_ascii_digit()).collect();
                if digits.len() >= 4 {
                    let h: i64 = digits[0..2].parse().unwrap_or(0);
                    let m: i64 = digits[2..4].parse().unwrap_or(0);
                    let sign = if bytes[0] == b'-' { -1 } else { 1 };
                    return sign * (h * 3600 + m * 60);
                }
            }
            0
        }
    }
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date (Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp as i64 + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc822_gmt() {
        assert_eq!(parse_rfc822("Tue, 10 Jun 2025 09:00:00 GMT"), Some(1_749_546_000));
    }

    #[test]
    fn rfc822_offset() {
        assert_eq!(parse_rfc822("Wed, 11 Jun 2025 18:30:00 +0200"), Some(1_749_659_400));
    }

    #[test]
    fn rfc822_epoch() {
        assert_eq!(parse_rfc822("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
    }

    #[test]
    fn rfc3339_basic() {
        assert_eq!(parse_rfc3339("2025-06-10T09:00:00Z"), Some(1_749_546_000));
    }

    #[test]
    fn rfc3339_offset_and_frac() {
        assert_eq!(parse_rfc3339("2025-06-11T18:30:00+02:00"), Some(1_749_659_400));
        assert_eq!(parse_rfc3339("2025-06-10T09:00:00.123Z"), Some(1_749_546_000));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_rfc822("not a date"), None);
        assert_eq!(parse_rfc3339("nope"), None);
    }
}
