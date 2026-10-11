//! arXiv Atom feed parsing.
//!
//! Pure, offline extraction of paper links from an arXiv Atom API response (the XML returned by
//! `http://export.arxiv.org/api/query`). No network, no IO: the caller fetches the body, this
//! module only transforms it, so it is fully testable offline and safe to call anywhere.

use std::collections::HashSet;

/// Opening tag prefix of an Atom `<entry>` element (namespace prefixes are tolerated because the
/// scan only looks for this substring, then the closing `>`).
const ENTRY_OPEN: &str = "<entry";
/// Closing tag of an Atom `<entry>` element.
const ENTRY_CLOSE: &str = "</entry>";
/// Opening tag of an Atom `<id>` element.
const ID_OPEN: &str = "<id>";
/// Closing tag of an Atom `<id>` element.
const ID_CLOSE: &str = "</id>";
/// Host/path marker shared by every arXiv abstract-page id, regardless of `http`/`https`.
const ARXIV_ABS_MARKER: &str = "arxiv.org/abs/";
/// Canonical prefix of the returned abstract-page URL.
const ARXIV_ABS_BASE: &str = "https://arxiv.org/abs/";

/// Extract every arXiv paper link from an arXiv Atom feed body.
///
/// Scans `<entry>` blocks for their `<id>`, keeps only ids that point at an arXiv abstract page,
/// strips any version suffix (e.g. `2401.12345v1` -> `2401.12345`), and returns the canonical
/// `https://arxiv.org/abs/...` URL. Results are deduplicated while preserving document order.
///
/// This is **pure**: it performs no network or filesystem IO, so it can be exercised offline.
/// An empty, truncated or malformed body yields an empty [`Vec`]; the function never panics.
///
/// ```
/// let body = "<feed><entry><id>http://arxiv.org/abs/2401.12345v1</id></entry></feed>";
/// assert_eq!(
///     search_sources::arxiv::parse_arxiv_atom(body),
///     vec!["https://arxiv.org/abs/2401.12345".to_string()],
/// );
/// ```
pub fn parse_arxiv_atom(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut rest = body;
    while let Some(open_at) = rest.find(ENTRY_OPEN) {
        let after_open = &rest[open_at + ENTRY_OPEN.len()..];
        let Some(gt_at) = after_open.find('>') else {
            break;
        };
        let after_gt = &after_open[gt_at + 1..];
        let Some(close_at) = after_gt.find(ENTRY_CLOSE) else {
            break;
        };
        let entry = &after_gt[..close_at];

        if let Some(url) = entry_abs_url(entry) {
            if seen.insert(url.clone()) {
                out.push(url);
            }
        }
        rest = &after_gt[close_at + ENTRY_CLOSE.len()..];
    }

    out
}

/// Pull the `<id>` text out of one `<entry>` block, if present.
fn entry_id(entry: &str) -> Option<&str> {
    let start = entry.find(ID_OPEN)? + ID_OPEN.len();
    let after = &entry[start..];
    let end = after.find(ID_CLOSE)?;
    Some(after[..end].trim())
}

/// Normalize a raw Atom id into a canonical arXiv abstract-page URL, or `None` when it is not an
/// arXiv abstract id. The version suffix is stripped.
fn entry_abs_url(entry: &str) -> Option<String> {
    let id = entry_id(entry)?;
    let marker_at = id.find(ARXIV_ABS_MARKER)?;
    let tail = &id[marker_at + ARXIV_ABS_MARKER.len()..];
    let tail = strip_version(tail.trim_end_matches('/'));
    if tail.is_empty() {
        return None;
    }
    Some(format!("{ARXIV_ABS_BASE}{tail}"))
}

/// Remove a trailing version suffix of the form `v<digits>` (case-insensitive). Non-version
/// endings are returned unchanged. Operates on ASCII delimiters only.
fn strip_version(id: &str) -> &str {
    let bytes = id.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i < bytes.len() && i > 0 && (bytes[i - 1] == b'v' || bytes[i - 1] == b'V') {
        &id[..i - 1]
    } else {
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_ENTRIES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>http://arxiv.org/api/feed-id</id>
  <title>ArXiv Query</title>
  <entry>
    <id>http://arxiv.org/abs/2401.12345v1</id>
    <title>First paper</title>
  </entry>
  <entry>
    <id>http://arxiv.org/abs/2402.00001v3</id>
    <title>Second paper</title>
  </entry>
</feed>"#;

    #[test]
    fn extracts_entries_in_order() {
        assert_eq!(
            parse_arxiv_atom(TWO_ENTRIES),
            vec![
                "https://arxiv.org/abs/2401.12345".to_string(),
                "https://arxiv.org/abs/2402.00001".to_string(),
            ]
        );
    }

    #[test]
    fn strips_version_suffix() {
        let body = "<feed><entry><id>http://arxiv.org/abs/hep-th/9901001v2</id></entry></feed>";
        assert_eq!(
            parse_arxiv_atom(body),
            vec!["https://arxiv.org/abs/hep-th/9901001".to_string()]
        );

        let no_version = "<feed><entry><id>https://arxiv.org/abs/2401.12345</id></entry></feed>";
        assert_eq!(
            parse_arxiv_atom(no_version),
            vec!["https://arxiv.org/abs/2401.12345".to_string()]
        );
    }

    #[test]
    fn deduplicates_preserving_first_occurrence() {
        let body = r#"<feed>
            <entry><id>http://arxiv.org/abs/2401.12345v1</id></entry>
            <entry><id>http://arxiv.org/abs/2402.00001</id></entry>
            <entry><id>http://arxiv.org/abs/2401.12345v7</id></entry>
        </feed>"#;
        assert_eq!(
            parse_arxiv_atom(body),
            vec![
                "https://arxiv.org/abs/2401.12345".to_string(),
                "https://arxiv.org/abs/2402.00001".to_string(),
            ]
        );
    }

    #[test]
    fn empty_truncated_and_entryless_bodies_are_empty() {
        assert!(parse_arxiv_atom("").is_empty());
        assert!(parse_arxiv_atom("   ").is_empty());
        assert!(
            parse_arxiv_atom("<feed><entry><id>http://arxiv.org/abs/2401.12345</id>").is_empty()
        );
        assert!(parse_arxiv_atom("<feed><id>http://arxiv.org/api/x</id></feed>").is_empty());
        assert!(parse_arxiv_atom("not xml at all").is_empty());
        assert!(parse_arxiv_atom("<feed><entry><title>no id</title></entry></feed>").is_empty());
    }
}
