//! MIME-based response router (plan Stage 1, crawler §19).
//!
//! Classifies a fetched response (Content-Type → magic-byte sniff → URL extension) into a
//! route so the pipeline can pick a parser. Interfaces frozen in `docs/STAGE1_SPEC.md`.
//!
//! Precedence: the declared `Content-Type` is preferred, but an *unambiguous* magic number in
//! the leading bytes always wins over a wrong declared type. The URL extension is consulted
//! only as a last resort (no usable header and no recognisable magic).

#![forbid(unsafe_code)]

/// The parser route a response is dispatched to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Html,
    Pdf,
    Feed,
    Json,
    Xml,
    PlainText,
    Image,
    Archive,
    /// Anything else, carrying the best available subtype hint (e.g. `"csv"`, `"unknown"`).
    Other(String),
}

/// Result of routing: the chosen route plus any charset declared by the Content-Type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routing {
    pub route: Route,
    pub charset: Option<String>,
}

/// Classify a response.
///
/// * `content_type` — the raw `Content-Type` header value, if any.
/// * `url` — the request/response URL (used only for extension fallback).
/// * `sniff` — leading bytes of the body (used for magic-number detection).
pub fn classify(content_type: Option<&str>, url: &str, sniff: &[u8]) -> Routing {
    let (declared, charset) = parse_content_type(content_type);

    // An unambiguous magic number beats a wrong (or missing) declared type.
    let route = match sniff_route(sniff) {
        Some(r) => r,
        None => match declared {
            Some(r) => r,
            None => route_from_url(url).unwrap_or_else(|| Route::Other("unknown".to_string())),
        },
    };

    Routing { route, charset }
}

/// Split a Content-Type into a declared [`Route`] and an optional charset.
///
/// Returns `(None, charset)` for an absent header or `application/octet-stream`, so the caller
/// falls through to magic-byte sniffing.
fn parse_content_type(content_type: Option<&str>) -> (Option<Route>, Option<String>) {
    let raw = match content_type {
        Some(v) if !v.trim().is_empty() => v,
        _ => return (None, None),
    };

    let mut parts = raw.split(';');
    let mime = parts.next().unwrap_or("").trim().to_ascii_lowercase();

    let mut charset = None;
    for param in parts {
        let param = param.trim();
        if let Some((k, v)) = param.split_once('=') {
            if k.trim().eq_ignore_ascii_case("charset") {
                let v = v.trim().trim_matches(|c| c == '"' || c == '\'').trim();
                if !v.is_empty() {
                    charset = Some(v.to_ascii_lowercase());
                }
            }
        }
    }

    let route = route_from_mime(&mime);
    (route, charset)
}

/// Map a bare (lowercased) media type to a route. `None` means "no usable declaration".
fn route_from_mime(mime: &str) -> Option<Route> {
    match mime {
        "" | "application/octet-stream" | "binary/octet-stream" => None,
        "text/html" | "application/xhtml+xml" => Some(Route::Html),
        "application/pdf" | "application/x-pdf" => Some(Route::Pdf),
        "text/plain" => Some(Route::PlainText),
        "application/json" | "text/json" => Some(Route::Json),
        "application/xml" | "text/xml" => Some(Route::Xml),
        "application/rss+xml" | "application/atom+xml" | "application/rdf+xml" => Some(Route::Feed),
        "text/csv" => Some(Route::Other("csv".to_string())),
        "application/zip" | "application/x-tar" | "application/gzip" | "application/x-gzip"
        | "application/x-7z-compressed" | "application/x-rar-compressed" => Some(Route::Archive),
        other => {
            if other.starts_with("image/") {
                Some(Route::Image)
            } else {
                // Keep an unknown declared type as an informative "other".
                Some(Route::Other(other.to_string()))
            }
        }
    }
}

/// Detect an unambiguous magic number in the leading bytes.
fn sniff_route(sniff: &[u8]) -> Option<Route> {
    let s = strip_bom(sniff);

    if s.starts_with(b"%PDF-") {
        return Some(Route::Pdf);
    }
    if s.starts_with(b"PK\x03\x04") {
        return Some(Route::Archive);
    }
    if s.starts_with(b"\xFF\xD8\xFF") || s.starts_with(b"\x89PNG") || s.starts_with(b"GIF8") {
        return Some(Route::Image);
    }

    let t = skip_ascii_ws(s);
    if t.starts_with(b"<?xml") {
        // Bound the search window so sniffing stays cheap on large buffers.
        let window = &s[..s.len().min(4096)];
        if contains(window, b"<rss") || contains(window, b"<feed") {
            return Some(Route::Feed);
        }
        return Some(Route::Xml);
    }
    if t.first() == Some(&b'{') || t.first() == Some(&b'[') {
        return Some(Route::Json);
    }

    None
}

/// Last-resort mapping from the URL path extension.
fn route_from_url(url: &str) -> Option<Route> {
    // Drop query/fragment, then take the extension of the final path segment.
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let ext = last.rsplit_once('.')?.1.to_ascii_lowercase();

    Some(match ext.as_str() {
        "html" | "htm" | "xhtml" => Route::Html,
        "pdf" => Route::Pdf,
        "json" => Route::Json,
        "xml" => Route::Xml,
        "rss" | "atom" | "rdf" => Route::Feed,
        "txt" | "text" => Route::PlainText,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "tif" | "tiff" => {
            Route::Image
        }
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" => Route::Archive,
        "csv" => Route::Other("csv".to_string()),
        _ => return None,
    })
}

fn strip_bom(b: &[u8]) -> &[u8] {
    b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(b)
}

fn skip_ascii_ws(b: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    &b[i..]
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(content_type: Option<&str>, url: &str, sniff: &[u8]) -> Route {
        classify(content_type, url, sniff).route
    }

    #[test]
    fn header_html() {
        assert_eq!(route(Some("text/html"), "", b""), Route::Html);
    }

    #[test]
    fn header_pdf() {
        assert_eq!(route(Some("application/pdf"), "", b""), Route::Pdf);
    }

    #[test]
    fn header_plain_text() {
        assert_eq!(route(Some("text/plain"), "", b""), Route::PlainText);
    }

    #[test]
    fn header_json() {
        assert_eq!(route(Some("application/json"), "", b""), Route::Json);
    }

    #[test]
    fn header_xml_and_feed() {
        assert_eq!(route(Some("application/xml"), "", b""), Route::Xml);
        assert_eq!(route(Some("text/xml"), "", b""), Route::Xml);
        assert_eq!(route(Some("application/rss+xml"), "", b""), Route::Feed);
        assert_eq!(route(Some("application/atom+xml"), "", b""), Route::Feed);
    }

    #[test]
    fn header_image_and_archive() {
        assert_eq!(route(Some("image/png"), "", b""), Route::Image);
        assert_eq!(route(Some("application/zip"), "", b""), Route::Archive);
    }

    #[test]
    fn header_csv_is_other() {
        assert_eq!(
            route(Some("text/csv"), "", b""),
            Route::Other("csv".to_string())
        );
    }

    #[test]
    fn header_case_insensitive_and_parameterised() {
        assert_eq!(
            route(Some("TEXT/HTML; charset=UTF-8"), "", b""),
            Route::Html
        );
    }

    #[test]
    fn charset_extraction_plain() {
        let r = classify(Some("text/html; charset=utf-8"), "", b"");
        assert_eq!(r.charset.as_deref(), Some("utf-8"));
    }

    #[test]
    fn charset_extraction_quoted_and_spaced() {
        let r = classify(Some("application/json;charset=\"ISO-8859-1\""), "", b"");
        assert_eq!(r.charset.as_deref(), Some("iso-8859-1"));
        assert_eq!(r.route, Route::Json);
    }

    #[test]
    fn charset_absent_is_none() {
        assert_eq!(classify(Some("text/html"), "", b"").charset, None);
    }

    #[test]
    fn magic_pdf_overrides_octet_stream() {
        assert_eq!(
            route(Some("application/octet-stream"), "http://x/y", b"%PDF-1.7\n..."),
            Route::Pdf
        );
    }

    #[test]
    fn magic_pdf_overrides_wrong_declared_html() {
        assert_eq!(
            route(Some("text/html"), "http://x/page.html", b"%PDF-1.4\n"),
            Route::Pdf
        );
    }

    #[test]
    fn magic_zip_overrides_declared_text() {
        assert_eq!(
            route(Some("text/plain"), "http://x/a", b"PK\x03\x04rest"),
            Route::Archive
        );
    }

    #[test]
    fn magic_json_and_xml() {
        assert_eq!(route(None, "", b"{\"a\":1}"), Route::Json);
        assert_eq!(route(None, "", b"  [1,2,3]"), Route::Json);
        assert_eq!(route(None, "", b"<?xml version=\"1.0\"?><root/>"), Route::Xml);
    }

    #[test]
    fn magic_feed_vs_xml() {
        assert_eq!(
            route(None, "", b"<?xml version=\"1.0\"?><rss version=\"2.0\"><channel/></rss>"),
            Route::Feed
        );
        assert_eq!(
            route(None, "", b"<?xml version=\"1.0\"?><feed xmlns=\"http://www.w3.org/2005/Atom\"/>"),
            Route::Feed
        );
    }

    #[test]
    fn magic_images() {
        assert_eq!(route(None, "", b"\xFF\xD8\xFF\xE0jpeg"), Route::Image);
        assert_eq!(route(None, "", b"\x89PNG\r\n\x1a\n"), Route::Image);
        assert_eq!(route(None, "", b"GIF89a..."), Route::Image);
    }

    #[test]
    fn magic_with_leading_whitespace_json() {
        assert_eq!(route(None, "", b"\n\t {\"k\": 1}"), Route::Json);
    }

    #[test]
    fn extension_fallback_only_when_header_absent() {
        assert_eq!(route(None, "http://x/a.PDF?v=1", b"random"), Route::Pdf);
        assert_eq!(route(Some("application/octet-stream"), "http://x/a.zip", b"\x00\x01"), Route::Archive);
        assert_eq!(route(None, "http://x/feed.rss", b"junk"), Route::Feed);
        assert_eq!(route(None, "http://x/pic.jpeg#frag", b"junk"), Route::Image);
    }

    #[test]
    fn extension_does_not_override_known_header() {
        // Declared HTML wins over a `.pdf`-looking URL when there is no magic.
        assert_eq!(route(Some("text/html"), "http://x/page.pdf", b"plain"), Route::Html);
    }

    #[test]
    fn unknown_is_other_unknown() {
        assert_eq!(
            route(None, "http://x/thing", b"some bytes"),
            Route::Other("unknown".to_string())
        );
        assert_eq!(route(None, "http://x/noext", b""), Route::Other("unknown".to_string()));
    }

    #[test]
    fn unknown_declared_type_keeps_subtype() {
        assert_eq!(
            route(Some("application/vnd.custom+thing"), "", b""),
            Route::Other("application/vnd.custom+thing".to_string())
        );
    }

    #[test]
    fn charset_preserved_even_when_magic_overrides() {
        let r = classify(Some("text/html; charset=utf-8"), "http://x", b"%PDF-1.4");
        assert_eq!(r.route, Route::Pdf);
        assert_eq!(r.charset.as_deref(), Some("utf-8"));
    }

    #[test]
    fn empty_content_type_falls_through_to_magic() {
        assert_eq!(route(Some("   "), "http://x/a.pdf", b""), Route::Pdf);
    }
}
