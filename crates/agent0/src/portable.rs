//! Compiling the live index into a portable dataset pack (and back).
//!
//! This module holds the one implementation of the *index → pack record* mapping. The
//! `agent0 export` CLI verb and the acceptance round-trip test both call it, so the raw
//! mapping cannot drift between the tool and its test.
//!
//! The source of truth is the Tantivy index, not the blob store: the schema `STOREs`
//! `url`/`title`/`body`/`source_id`/`published_at`, which is exactly what a pack record needs.
//! The mapping is therefore lossy on the fields the schema does not store (`headings`,
//! `entities`) — by design, and documented in `docs/PORTABLE_MODEL.md`.

use search_dataset::Record;
use search_indexer::IndexDoc;
use search_organize::RawPage;

/// Map index docs to the raw pages the organizer (or the `--raw` path) consumes.
///
/// Returns the pages plus the number of docs skipped because they carry no usable
/// url/text. `limit` caps the number of pages produced.
pub fn pages_from_docs(docs: &[IndexDoc], limit: usize) -> (Vec<RawPage>, usize) {
    let mut pages = Vec::new();
    let mut skipped = 0usize;
    for d in docs {
        if pages.len() >= limit {
            break;
        }
        if d.url.trim().is_empty() || d.body.trim().is_empty() {
            skipped += 1;
            continue;
        }
        pages.push(RawPage {
            url: d.url.clone(),
            source: "web".to_string(),
            title: (!d.title.trim().is_empty()).then(|| d.title.clone()),
            text: d.body.clone(),
            published_at: d.published_at,
        });
    }
    (pages, skipped)
}

/// One raw page → one pack record carrying the crawler's own fields (no model involved).
///
/// The record id matches the organizer's scheme (`search_organize::id_from_url`) so a raw
/// export and an organized export of the same page address the same record.
pub fn raw_record(page: &RawPage) -> Record {
    let mut record = Record::new(search_organize::id_from_url(&page.url), page.source.clone());
    record.url = Some(page.url.clone());
    record.title = page.title.clone();
    record.text = page.text.clone();
    record.published_at = page.published_at;
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(url: &str, title: &str, body: &str) -> IndexDoc {
        IndexDoc::new(format!("doc:{url}"), format!("res:{url}"), url, title, body)
    }

    #[test]
    fn pages_skip_docs_without_url_or_body() {
        let docs = vec![
            doc("https://a.example/", "A", "alpha body"),
            doc("", "no url", "body"),
            doc("https://b.example/", "B", "   "),
            doc("https://c.example/", "C", "gamma body"),
        ];
        let (pages, skipped) = pages_from_docs(&docs, usize::MAX);
        assert_eq!(pages.len(), 2);
        assert_eq!(skipped, 2);
        assert_eq!(pages[0].url, "https://a.example/");
        assert_eq!(pages[1].url, "https://c.example/");
    }

    #[test]
    fn raw_record_id_matches_the_organizer_scheme() {
        let page = RawPage {
            url: "https://a.example/".to_string(),
            source: "web".to_string(),
            title: Some("A".to_string()),
            text: "alpha".to_string(),
            published_at: Some(1_700_000_000),
        };
        let r = raw_record(&page);
        assert_eq!(r.id, search_organize::id_from_url("https://a.example/"));
        assert_eq!(r.url.as_deref(), Some("https://a.example/"));
        assert_eq!(r.published_at, Some(1_700_000_000));
    }
}
