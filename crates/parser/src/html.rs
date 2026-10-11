//! HTML normalization: extract title, visible text, headings, entities and
//! outlinks. Deliberately simple and deterministic for Stage 0 — no readability
//! heuristics, no boilerplate removal (that is a later stage).

use scraper::{Html, Selector};
use search_core::{AppError, AppResult};
use serde::{Deserialize, Serialize};

/// The normalized projection of one HTML response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedDoc {
    pub url: String,
    /// `<title>`, else the first `<h1>`, else `None`.
    pub title: Option<String>,
    /// Concatenated visible text with collapsed whitespace.
    pub text: String,
    /// Heading texts in document order (`h1..h6`).
    pub headings: Vec<String>,
    /// Candidate entities: capitalized multi-word spans from headings/title.
    /// Stage 0 heuristic only; a real NER pass replaces this later.
    pub entities: Vec<String>,
    pub language: Option<String>,
    /// `<meta name="description">` when present.
    pub description: Option<String>,
    /// `article:published_time` / `<time datetime>` when present (Unix seconds).
    pub published_at: Option<i64>,
    pub modified_at: Option<i64>,
    /// Absolute outlinks discovered in document order, de-duplicated.
    pub outlinks: Vec<String>,
    /// `html` for Stage 0.
    pub doc_type: String,
    /// Byte length of the normalized text.
    pub text_len: usize,
}

/// Parse a body as UTF-8 HTML (lossy) and normalize it. `url` is used to
/// resolve relative outlinks to absolute URLs.
pub fn normalize_html(url: &str, body: &[u8]) -> AppResult<NormalizedDoc> {
    let base = url::Url::parse(url).map_err(|e| AppError::Invalid(format!("bad url: {e}")))?;
    let text = String::from_utf8_lossy(body);
    let doc = Html::parse_document(&text);

    let title = select_first_text(&doc, "title").or_else(|| select_first_text(&doc, "h1"));

    let headings = collect_all_text(
        &doc,
        "h1, h2, h3, h4, h5, h6",
    );

    let language = doc
        .select(&sel("html"))
        .next()
        .and_then(|e| e.value().attr("lang"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let description = doc
        .select(&sel("meta[name=description]"))
        .next()
        .and_then(|e| e.value().attr("content"))
        .map(|s| collapse(s))
        .filter(|s| !s.is_empty());

    let published_at = select_first_attr(&doc, r#"meta[property="article:published_time"]"#, "content")
        .or_else(|| select_first_attr(&doc, "time[datetime]", "datetime"))
        .and_then(|s| parse_ts(&s));
    let modified_at = select_first_attr(&doc, r#"meta[property="article:modified_time"]"#, "content")
        .and_then(|s| parse_ts(&s));

    let outlinks = collect_outlinks(&doc, &base);

    let body_text = visible_text(&doc);
    let mut entities = Vec::new();
    if let Some(t) = &title {
        entities.extend(multiword_capitalized(t));
    }
    for h in &headings {
        entities.extend(multiword_capitalized(h));
    }
    entities.sort();
    entities.dedup();

    Ok(NormalizedDoc {
        url: url.to_string(),
        title,
        text_len: body_text.len(),
        text: body_text,
        headings,
        entities,
        language,
        description,
        published_at,
        modified_at,
        outlinks,
        doc_type: "html".to_string(),
    })
}

fn sel(pattern: &str) -> Selector {
    Selector::parse(pattern).expect("static selector")
}

fn select_first_text(doc: &Html, pattern: &str) -> Option<String> {
    doc.select(&sel(pattern))
        .next()
        .map(|e| collapse(&e.text().collect::<String>()))
        .filter(|s| !s.is_empty())
}

fn collect_all_text(doc: &Html, pattern: &str) -> Vec<String> {
    doc.select(&sel(pattern))
        .map(|e| collapse(&e.text().collect::<String>()))
        .filter(|s| !s.is_empty())
        .collect()
}

fn select_first_attr(doc: &Html, pattern: &str, attr: &str) -> Option<String> {
    doc.select(&sel(pattern))
        .next()
        .and_then(|e| e.value().attr(attr))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Visible text = body text with `<script>`/`<style>`/`<noscript>` removed.
fn visible_text(doc: &Html) -> String {
    let body = match doc.select(&sel("body")).next() {
        Some(b) => b,
        None => {
            return collapse(&doc.root_element().text().collect::<String>());
        }
    };
    let mut out = String::new();
    for node in body.descendants() {
        if let Some(t) = node.value().as_text() {
            // Skip text inside script/style/noscript/template subtrees.
            let in_hidden = node.ancestors().any(|a| {
                a.value()
                    .as_element()
                    .map(|el| matches!(el.name(), "script" | "style" | "noscript" | "template"))
                    .unwrap_or(false)
            });
            if in_hidden {
                continue;
            }
            out.push_str(t);
            out.push(' ');
        }
    }
    collapse(&out)
}

fn collect_outlinks(doc: &Html, base: &url::Url) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for a in doc.select(&sel("a[href]")) {
        let Some(href) = a.value().attr("href") else { continue };
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            continue;
        }
        let Ok(abs) = base.join(href) else { continue };
        if !matches!(abs.scheme(), "http" | "https") {
            continue;
        }
        let mut s = abs.to_string();
        // drop the fragment: it does not identify a distinct resource here
        if let Some(pos) = s.find('#') {
            s.truncate(pos);
        }
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out
}

/// Candidate entities: runs of 2..=4 capitalized words (Stage-0 heuristic).
fn multiword_capitalized(s: &str) -> Vec<String> {
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut out = Vec::new();
    let mut run: Vec<&str> = Vec::new();
    let flush = |run: &mut Vec<&str>, out: &mut Vec<String>| {
        if run.len() >= 2 {
            out.push(run.join(" "));
        }
        run.clear();
    };
    for w in words {
        let cap = w.chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
        if cap && run.len() < 4 {
            run.push(w);
        } else {
            flush(&mut run, &mut out);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn parse_ts(s: &str) -> Option<i64> {
    // Accept RFC 3339 / ISO 8601 date or datetime; `time` crate is not in the
    // pinned set, so parse the common `YYYY-MM-DD[THH:MM:SS...]` shape by hand.
    //
    // The input is arbitrary page text: a `<time datetime>` attribute or a `meta[content]` value
    // has no schema behind it. A page whose value begins with a multi-byte character (an Arabic
    // locale page, for instance) has no char boundary at byte 10, and `split_at(10)` PANICKED —
    // "end byte index 10 is not a char boundary; it is inside 'ي'" — which killed the whole crawl
    // task, not just the document. Every slice below is a checked `get`.
    let s = s.trim();
    if s.len() < 10 {
        return None;
    }
    let (date, rest) = s.split_at_checked(10)?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: i64 = d.next()?.parse().ok()?;
    let da: i64 = d.next()?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }
    let (hh, mm, ss) = if let Some(t) = rest.get(1..9) {
        let mut it = t.split(':');
        (
            it.next().and_then(|v| v.parse().ok()).unwrap_or(0i64),
            it.next().and_then(|v| v.parse().ok()).unwrap_or(0i64),
            it.next().and_then(|v| v.parse().ok()).unwrap_or(0i64),
        )
    } else {
        (0, 0, 0)
    };
    Some(days_from_civil(y, mo, da) * 86400 + hh * 3600 + mm * 60 + ss)
}

/// Days since 1970-01-01 (Howard Hinnant's civil-date algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Collapse all whitespace runs to single spaces and trim.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r##"<!doctype html>
<html lang="en"><head>
<title>Ada Lovelace and the Analytical Engine</title>
<meta name="description" content="A short biography.">
<meta property="article:published_time" content="2024-03-05T10:00:00Z">
</head><body>
<h1>Ada Lovelace</h1>
<script>var x = "should not appear";</script>
<p>Ada Lovelace worked with Charles Babbage on the Analytical Engine.</p>
<a href="/wiki/Charles_Babbage">Charles Babbage</a>
<a href="#cite-1">ref</a>
<a href="https://example.org/x#frag">External</a>
</body></html>"##;

    #[test]
    fn extracts_title_headings_and_text() {
        let d = normalize_html("https://en.wikipedia.org/wiki/Ada_Lovelace", PAGE.as_bytes()).unwrap();
        assert_eq!(d.title.as_deref(), Some("Ada Lovelace and the Analytical Engine"));
        assert!(d.headings.contains(&"Ada Lovelace".to_string()));
        assert!(d.text.contains("Analytical Engine"));
        assert!(!d.text.contains("should not appear"), "script text leaked");
        assert_eq!(d.doc_type, "html");
        assert_eq!(d.language.as_deref(), Some("en"));
    }

    #[test]
    fn resolves_and_dedupes_outlinks() {
        let d = normalize_html("https://en.wikipedia.org/wiki/Ada_Lovelace", PAGE.as_bytes()).unwrap();
        assert!(d.outlinks.contains(&"https://en.wikipedia.org/wiki/Charles_Babbage".to_string()));
        assert!(d.outlinks.contains(&"https://example.org/x".to_string()));
        assert!(!d.outlinks.iter().any(|u| u.contains('#')));
    }

    #[test]
    fn parses_published_time() {
        let d = normalize_html("https://x.test/a", PAGE.as_bytes()).unwrap();
        // 2024-03-05T10:00:00Z
        assert_eq!(d.published_at, Some(1_709_632_800));
    }

    #[test]
    fn a_multibyte_datetime_is_rejected_not_a_panic() {
        // Live regression (shard 3, https://www.premierleague.com/ar/...): a `<time datetime>`
        // value that begins with a multi-byte character has no char boundary at byte 10, so
        // `split_at(10)` aborted the crawl task: "end byte index 10 is not a char boundary".
        // The document must simply carry no timestamp.
        let page = r#"<html><body>
            <h1>خبر</h1>
            <time datetime="يييييييييييي">الآن</time>
            <p>text body long enough to normalise</p>
        </body></html>"#;
        let d = normalize_html("https://x.test/ar", page.as_bytes()).unwrap();
        assert_eq!(d.published_at, None);
        assert_eq!(d.modified_at, None);

        // A multi-byte character ending exactly at byte 9 of the time part must not panic either.
        // (It is not a valid time, so the clock fields default to 0 and the date part stands.)
        let page = r#"<html><body>
            <time datetime="2024-03-05T日日日Z">x</time>
            <p>text body long enough to normalise</p>
        </body></html>"#;
        let d = normalize_html("https://x.test/ar2", page.as_bytes()).unwrap();
        assert_eq!(d.published_at, Some(1_709_596_800)); // 2024-03-05T00:00:00Z

        // And the ASCII shape still parses.
        let page = r#"<html><body>
            <time datetime="2024-03-05T10:00:00Z">x</time>
            <p>text body long enough to normalise</p>
        </body></html>"#;
        let d = normalize_html("https://x.test/ok", page.as_bytes()).unwrap();
        assert_eq!(d.published_at, Some(1_709_632_800));
    }

    #[test]
    fn entities_are_multiword_capitalized() {
        let d = normalize_html("https://x.test/a", PAGE.as_bytes()).unwrap();
        assert!(d.entities.contains(&"Ada Lovelace".to_string()), "got {:?}", d.entities);
    }

    #[test]
    fn router_rejects_unknown_types() {
        assert!(crate::normalize("https://x.test/a", b"xx", Some("application/pdf")).is_err());
        assert!(crate::normalize("https://x.test/a", PAGE.as_bytes(), Some("text/html")).is_ok());
    }
}
