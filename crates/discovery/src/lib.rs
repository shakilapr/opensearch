//! Sitemap and RSS/Atom feed discovery (plan Stage 1, crawler §7–8).
//!
//! Turns sitemaps, sitemap indexes, and feeds into discovered-resource records so the
//! crawler learns about URLs without fetching every page. Pure parsing: no I/O.
//!
//! Interfaces are frozen in `docs/STAGE1_SPEC.md`. Module layout is up to the owner.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::io::Read;

use thiserror::Error;

mod dates;
mod feed;
mod sitemap;

/// Hard cap on resources returned by a single parse call (spec bound).
pub const MAX_RESOURCES: usize = 50_000;
/// Hard cap on a decompressed document body (spec bound: 32 MiB).
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

/// What a discovered URL is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    Page,
    FeedEntry,
    Sitemap,
    Image,
    Other,
}

/// A single URL learned from a sitemap or feed.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredResource {
    pub url: String,
    pub lastmod: Option<String>,
    pub changefreq: Option<String>,
    pub priority: Option<f32>,
    pub kind: ResourceKind,
    pub title: Option<String>,
    pub published_at: Option<i64>,
    pub source_url: String,
}

/// Errors produced while parsing discovery documents.
#[derive(Debug, Error)]
pub enum DiscoverError {
    #[error("xml parse error: {0}")]
    Xml(String),
    #[error("body too large: {0} bytes")]
    TooLarge(usize),
    #[error("unsupported document: {0}")]
    Unsupported(String),
}

/// Parse a sitemap.xml (`<urlset>`) or sitemap index (`<sitemapindex>`).
///
/// Handles gzip-compressed bodies (detected by the `0x1f 0x8b` magic) and resolves
/// relative `<loc>` values against `source_url`.
pub fn parse_sitemap(xml: &[u8], source_url: &str) -> Result<Vec<DiscoveredResource>, DiscoverError> {
    let body = decompress(xml)?;
    sitemap::parse(&body, source_url)
}

/// Child-sitemap URLs from a sitemap index (`<sitemapindex>`).
///
/// A document that is not a sitemap index yields an empty vector.
pub fn sitemap_index_urls(xml: &[u8]) -> Result<Vec<String>, DiscoverError> {
    let body = decompress(xml)?;
    let resources = sitemap::parse(&body, "")?;
    Ok(resources
        .into_iter()
        .filter(|r| r.kind == ResourceKind::Sitemap)
        .map(|r| r.url)
        .collect())
}

/// Sitemap URLs advertised in a robots.txt body (`Sitemap:` lines, case-insensitive).
///
/// Values are trimmed and normalized to an absolute URL when they parse as one.
pub fn sitemaps_in_robots(robots_txt: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in robots_txt.lines() {
        let line = line.trim();
        // Skip comments; a directive is `key: value`.
        let line = line.split('#').next().unwrap_or(line).trim();
        if let Some((key, value)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case("sitemap") {
                let value = value.trim();
                if !value.is_empty() {
                    out.push(normalize_absolute(value));
                }
            }
        }
    }
    out
}

/// Parse RSS 2.0 (`<item>`, RFC 822 `<pubDate>`) or Atom 1.0
/// (`<entry>`, RFC 3339 `<published>`/`<updated>`).
pub fn parse_feed(xml: &[u8], source_url: &str) -> Result<Vec<DiscoveredResource>, DiscoverError> {
    let body = decompress(xml)?;
    feed::parse(&body, source_url)
}

/// Return an absolute, normalized URL string, or the trimmed input when it is not
/// an absolute URL (e.g. a bare path with no origin to resolve against).
fn normalize_absolute(value: &str) -> String {
    match url::Url::parse(value) {
        Ok(u) => u.to_string(),
        Err(_) => value.to_string(),
    }
}

/// Decompress a gzip body when the gzip magic is present, enforcing `MAX_BYTES`
/// on the decompressed output. Plain bodies are returned borrowed, also size-checked.
fn decompress(body: &[u8]) -> Result<Cow<'_, [u8]>, DiscoverError> {
    if body.len() >= 2 && body[0] == 0x1f && body[1] == 0x8b {
        let mut decoder = flate2::read::GzDecoder::new(body);
        let mut out = Vec::new();
        // Read at most one byte past the cap so an oversized body is detectable.
        let mut limited = (&mut decoder).take(MAX_BYTES as u64 + 1);
        limited
            .read_to_end(&mut out)
            .map_err(|e| DiscoverError::Xml(e.to_string()))?;
        if out.len() > MAX_BYTES {
            return Err(DiscoverError::TooLarge(out.len()));
        }
        Ok(Cow::Owned(out))
    } else {
        if body.len() > MAX_BYTES {
            return Err(DiscoverError::TooLarge(body.len()));
        }
        Ok(Cow::Borrowed(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gzip(data: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    const URLSET: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://example.com/a</loc><lastmod>2024-01-01</lastmod></url>
  <url><loc>https://example.com/b</loc></url>
  <url><loc>https://example.com/c</loc><lastmod>2024-03-04T10:00:00Z</lastmod></url>
</urlset>"#;

    #[test]
    fn urlset_with_three_urls_and_lastmod() {
        let res = parse_sitemap(URLSET.as_bytes(), "https://example.com/sitemap.xml").unwrap();
        assert_eq!(res.len(), 3);
        assert_eq!(res[0].url, "https://example.com/a");
        assert_eq!(res[0].lastmod.as_deref(), Some("2024-01-01"));
        assert_eq!(res[0].kind, ResourceKind::Page);
        assert_eq!(res[1].lastmod, None);
        assert_eq!(res[2].lastmod.as_deref(), Some("2024-03-04T10:00:00Z"));
        assert!(res.iter().all(|r| r.source_url == "https://example.com/sitemap.xml"));
    }

    const SITEMAPINDEX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <sitemap><loc>https://example.com/sitemap-1.xml</loc><lastmod>2024-01-01</lastmod></sitemap>
  <sitemap><loc>https://example.com/sitemap-2.xml</loc></sitemap>
</sitemapindex>"#;

    #[test]
    fn sitemapindex_yields_two_children() {
        let res = parse_sitemap(SITEMAPINDEX.as_bytes(), "https://example.com/sitemap_index.xml").unwrap();
        assert_eq!(res.len(), 2);
        assert!(res.iter().all(|r| r.kind == ResourceKind::Sitemap));
        assert_eq!(res[0].url, "https://example.com/sitemap-1.xml");
        assert_eq!(res[0].lastmod.as_deref(), Some("2024-01-01"));

        let urls = sitemap_index_urls(SITEMAPINDEX.as_bytes()).unwrap();
        assert_eq!(urls, vec![
            "https://example.com/sitemap-1.xml".to_string(),
            "https://example.com/sitemap-2.xml".to_string(),
        ]);
    }

    #[test]
    fn gzip_compressed_sitemap() {
        let gz = gzip(URLSET.as_bytes());
        assert_eq!(&gz[..2], &[0x1f, 0x8b]);
        let res = parse_sitemap(&gz, "https://example.com/sitemap.xml.gz").unwrap();
        assert_eq!(res.len(), 3);
        assert_eq!(res[1].url, "https://example.com/b");
    }

    #[test]
    fn robots_with_two_sitemap_lines() {
        let robots = "\
User-agent: *
Disallow: /private

Sitemap: https://example.com/sitemap.xml
sitemap: https://example.com/news.xml
";
        let urls = sitemaps_in_robots(robots);
        assert_eq!(urls, vec![
            "https://example.com/sitemap.xml".to_string(),
            "https://example.com/news.xml".to_string(),
        ]);
    }

    #[test]
    fn relative_loc_resolution() {
        let xml = "<urlset><url><loc>/about</loc></url><url><loc>contact.html</loc></url></urlset>";
        let res = parse_sitemap(xml.as_bytes(), "https://example.com/sitemap.xml").unwrap();
        assert_eq!(res[0].url, "https://example.com/about");
        assert_eq!(res[1].url, "https://example.com/contact.html");
    }

    #[test]
    fn changefreq_and_priority_are_captured() {
        let xml = r#"<urlset><url><loc>https://e.com/x</loc><changefreq>daily</changefreq><priority>0.8</priority></url></urlset>"#;
        let res = parse_sitemap(xml.as_bytes(), "https://e.com/sitemap.xml").unwrap();
        assert_eq!(res[0].changefreq.as_deref(), Some("daily"));
        assert_eq!(res[0].priority, Some(0.8_f32));
    }

    #[test]
    fn malformed_xml_is_an_xml_error() {
        let err = parse_sitemap(b"<urlset><url><loc>https://e.com/x</urlset>", "https://e.com/sitemap.xml")
            .unwrap_err();
        assert!(matches!(err, DiscoverError::Xml(_)));
    }

    #[test]
    fn oversized_body_is_too_large() {
        let big = vec![b' '; 33 * 1024 * 1024];
        let err = parse_sitemap(&big, "https://e.com/sitemap.xml").unwrap_err();
        assert!(matches!(err, DiscoverError::TooLarge(_)));
    }

    #[test]
    fn oversized_gzip_body_is_too_large() {
        // Highly compressible payload -> small gzip, huge decompressed body.
        let big = vec![b'a'; 33 * 1024 * 1024];
        let gz = gzip(&big);
        assert!(gz.len() < 1024 * 1024);
        let err = parse_sitemap(&gz, "https://e.com/sitemap.xml.gz").unwrap_err();
        assert!(matches!(err, DiscoverError::TooLarge(_)));
    }

    #[test]
    fn non_sitemap_document_is_unsupported() {
        let err = parse_sitemap(b"<html><body>hi</body></html>", "https://e.com/x").unwrap_err();
        assert!(matches!(err, DiscoverError::Unsupported(_)));
    }

    // ---- feeds ----

    const RSS2: &str = r#"<?xml version="1.0"?>
<rss version="2.0">
  <channel>
    <title>Example News</title>
    <link>https://example.com/</link>
    <item>
      <title>First post</title>
      <link>https://example.com/posts/1</link>
      <guid>https://example.com/posts/1</guid>
      <pubDate>Tue, 10 Jun 2025 09:00:00 GMT</pubDate>
    </item>
    <item>
      <title>Second post</title>
      <link>https://example.com/posts/2</link>
      <pubDate>Wed, 11 Jun 2025 18:30:00 +0200</pubDate>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn rss2_with_pubdate() {
        let res = parse_feed(RSS2.as_bytes(), "https://example.com/feed.xml").unwrap();
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].kind, ResourceKind::FeedEntry);
        assert_eq!(res[0].url, "https://example.com/posts/1");
        assert_eq!(res[0].title.as_deref(), Some("First post"));
        assert_eq!(res[0].published_at, Some(1_749_546_000)); // 2025-06-10T09:00:00Z
        // 2025-06-11T18:30:00+02:00 == 2025-06-11T16:30:00Z
        assert_eq!(res[1].published_at, Some(1_749_659_400));
    }

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Example Blog</title>
  <entry>
    <title>Hello World</title>
    <link rel="alternate" href="https://example.com/hello"/>
    <id>tag:example.com,2025:hello</id>
    <published>2025-06-10T09:00:00Z</published>
    <updated>2025-06-11T10:00:00Z</updated>
  </entry>
  <entry>
    <title>No published</title>
    <link href="https://example.com/second"/>
    <updated>2025-06-12T00:00:00Z</updated>
  </entry>
</feed>"#;

    #[test]
    fn atom_with_published() {
        let res = parse_feed(ATOM.as_bytes(), "https://example.com/atom.xml").unwrap();
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].url, "https://example.com/hello");
        assert_eq!(res[0].title.as_deref(), Some("Hello World"));
        assert_eq!(res[0].published_at, Some(1_749_546_000));
        // Falls back to <updated> when <published> is absent.
        assert_eq!(res[1].url, "https://example.com/second");
        assert_eq!(res[1].published_at, Some(1_749_686_400));
    }

    #[test]
    fn feed_relative_link_resolution() {
        let xml = r#"<rss version="2.0"><channel>
          <item><title>Local</title><link>/local-post</link></item>
        </channel></rss>"#;
        let res = parse_feed(xml.as_bytes(), "https://example.com/feed.xml").unwrap();
        assert_eq!(res[0].url, "https://example.com/local-post");
    }
}
