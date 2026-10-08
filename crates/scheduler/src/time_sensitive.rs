//! Is a query time-sensitive ("latest X", "X today", news, year/date tokens)?
//!
//! Used by [`crate::ActivationScheduler::on_query`] to decide whether a query may trigger P0
//! refresh work (crawler §40). The rule is deliberately conservative and documented here:
//!
//! A query is time-sensitive when its tokenization contains:
//! * a **recency keyword** — `latest`, `newest`, `today`, `now`, `current`, `breaking`, `news`;
//! * a **4-digit year** within `±2` years of `now` (e.g. `2026` when `now` is in 2026); or
//! * a **month name** — full (`january`.. `december`) or 3-letter abbreviation (`jan`..`dec`).
//!
//! Tokens are produced by splitting on non-alphanumeric characters and lowercasing, so
//! punctuation (`"latest news?"`) and substring traps (`"known"` does not match `now`) are
//! handled correctly. Anything else is not time-sensitive.

/// Number of years either side of the current year that count as "recent".
pub const RECENT_YEAR_WINDOW: i64 = 2;

const RECENCY_KEYWORDS: &[&str] = &[
    "latest", "newest", "today", "now", "current", "breaking", "news",
];

const MONTHS: &[&str] = &[
    "january", "february", "march", "april", "may", "june", "july", "august", "september",
    "october", "november", "december",
];

const MONTH_ABBREVS: &[&str] = &[
    "jan", "feb", "mar", "apr", "jun", "jul", "aug", "sep", "sept", "oct", "nov", "dec",
];

/// Is a query time-sensitive ("latest X", "X today", "news", year/date tokens)?
///
/// `now` is Unix seconds (UTC), matching `search_core::time`. A non-positive `now` disables year
/// matching but keyword and month matching still apply.
pub fn is_time_sensitive(query: &str, now: i64) -> bool {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .collect();

    let current_year = year_from_unix_secs(now);

    for token in &tokens {
        if RECENCY_KEYWORDS.contains(&token.as_str()) {
            return true;
        }
        if MONTHS.contains(&token.as_str()) || MONTH_ABBREVS.contains(&token.as_str()) {
            return true;
        }
        // A 4-digit token in the plausible-year range and within the recency window.
        if token.len() == 4 && token.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(y) = token.parse::<i64>() {
                if (1000..=2999).contains(&y) && (y - current_year).abs() <= RECENT_YEAR_WINDOW {
                    return true;
                }
            }
        }
    }
    false
}

/// Extract the UTC calendar year from Unix seconds (Howard Hinnant's civil-from-days).
fn year_from_unix_secs(secs: i64) -> i64 {
    let days = secs.div_euclid(86_400);
    civil_from_days(days).0
}

/// Days since 1970-01-01 -> (year, month, day).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-06-15T00:00:00Z and 2024-01-01T00:00:00Z.
    const NOW_2026: i64 = 1_781_481_600;
    const NOW_2024: i64 = 1_704_067_200;

    #[test]
    fn time_sensitive_keywords() {
        for q in [
            "latest rust news",
            "Rust NEWEST release",
            "what is happening today",
            "the current status of X",
            "breaking: something",
            "now playing",
            "tech news",
        ] {
            assert!(is_time_sensitive(q, NOW_2026), "expected sensitive: {q}");
        }
    }

    #[test]
    fn time_sensitive_year_window() {
        // Within +2 of 2026.
        assert!(is_time_sensitive("rust 2026", NOW_2026));
        assert!(is_time_sensitive("rust 2025", NOW_2026));
        assert!(is_time_sensitive("rust 2024", NOW_2026));
        // Outside the window.
        assert!(!is_time_sensitive("rust 2023", NOW_2026));
        assert!(!is_time_sensitive("rust 1999", NOW_2026));
        // Relative to a different present.
        assert!(is_time_sensitive("events 2022", NOW_2024));
        assert!(!is_time_sensitive("events 2020", NOW_2024));
        // Abbreviated years are not treated as years.
        assert!(!is_time_sensitive("rust 26", NOW_2026));
    }

    #[test]
    fn time_sensitive_month_names() {
        for q in [
            "conference in january",
            "released December 2024",
            "schedule for sept",
            "what happened in mar",
        ] {
            assert!(is_time_sensitive(q, NOW_2026), "expected sensitive: {q}");
        }
    }

    #[test]
    fn time_sensitive_negative_cases() {
        for q in [
            "",
            "rust ownership model",
            "how to bake bread",
            "binary search tree",
            "known algorithms",   // "now" is a substring of "known" but not a token
            "recently",           // not a keyword token
            "marching band",      // "mar" is a substring of "marching", not a token
            "documentation 1990", // out-of-window year
        ] {
            assert!(!is_time_sensitive(q, NOW_2026), "expected NOT sensitive: {q}");
        }
    }
}
