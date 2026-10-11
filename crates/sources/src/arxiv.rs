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

/// Error returned by the injectable HTTP transport.
#[derive(Debug, Clone, thiserror::Error)]
pub enum FetchError {
    /// The transport could not complete the GET (connection, timeout, non-2xx, decode).
    #[error("http transport error: {0}")]
    Http(String),
}

/// The one method the arXiv/OAI fetch path needs: `GET url -> body`.
///
/// Injected so the wrapper can be exercised with a canned body and **no network**. Production
/// supplies a `reqwest`-backed implementation; tests supply a stub.
#[async_trait::async_trait]
pub trait HttpGet: Send + Sync {
    /// Fetch `url` and return the response body as UTF-8 text.
    async fn get(&self, url: &str) -> Result<String, FetchError>;
}

/// Base of the arXiv Atom query API.
pub const ARXIV_QUERY_ENDPOINT: &str = "http://export.arxiv.org/api/query";

/// Build the arXiv API query URL for `search_query`, asking for `max_results` entries from `start`.
///
/// Parameters are percent-encoded, so a query like `all:electron & photon` is safe to embed.
pub fn arxiv_query_url(search_query: &str, max_results: usize, start: usize) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("search_query", search_query)
        .append_pair("start", &start.to_string())
        .append_pair("max_results", &max_results.to_string())
        .finish();
    format!("{ARXIV_QUERY_ENDPOINT}?{query}")
}

/// Fetch an arXiv Atom feed through `client` and return the canonical paper links.
///
/// Thin wrapper: build the query URL, fetch it, run the pure [`parse_arxiv_atom`]. No network
/// logic lives here beyond the injected transport, so tests stay hermetic.
pub async fn fetch_arxiv_links<C: HttpGet + ?Sized>(
    client: &C,
    search_query: &str,
    max_results: usize,
) -> Result<Vec<String>, FetchError> {
    let url = arxiv_query_url(search_query, max_results, 0);
    let body = client.get(&url).await?;
    Ok(parse_arxiv_atom(&body))
}

/// One OAI-PMH repository endpoint plus an optional set spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OaiRepository {
    /// Short human-readable name (used only for logging/attribution).
    pub name: String,
    /// OAI-PMH endpoint base, e.g. `http://export.arxiv.org/oai2`.
    pub base_url: String,
    /// Optional OAI `set` to restrict the harvest.
    pub set: Option<String>,
}

impl OaiRepository {
    /// A repository with no set restriction.
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            set: None,
        }
    }

    /// Attach a set spec.
    pub fn with_set(mut self, set: impl Into<String>) -> Self {
        self.set = Some(set.into());
        self
    }
}

/// The default repository set: arXiv's OAI-PMH endpoint.
pub fn default_oai_repositories() -> Vec<OaiRepository> {
    vec![OaiRepository::new("arxiv", "http://export.arxiv.org/oai2")]
}

/// Build one OAI-PMH `ListRecords` URL (`metadataPrefix=oai_dc`) for `repo`, with optional
/// `from`/`until` datestamps. Empty/whitespace bounds and sets are omitted.
pub fn oai_list_records_url(
    repo: &OaiRepository,
    from: Option<&str>,
    until: Option<&str>,
) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    q.append_pair("verb", "ListRecords");
    q.append_pair("metadataPrefix", "oai_dc");
    if let Some(set) = repo.set.as_deref().filter(|s| !s.trim().is_empty()) {
        q.append_pair("set", set);
    }
    if let Some(f) = from.filter(|s| !s.trim().is_empty()) {
        q.append_pair("from", f);
    }
    if let Some(u) = until.filter(|s| !s.trim().is_empty()) {
        q.append_pair("until", u);
    }
    join_query(&repo.base_url, &q.finish())
}

/// Build `ListRecords` URLs for every repository in `repos`, in order.
pub fn oai_list_records_urls(
    repos: &[OaiRepository],
    from: Option<&str>,
    until: Option<&str>,
) -> Vec<String> {
    repos
        .iter()
        .map(|r| oai_list_records_url(r, from, until))
        .collect()
}

/// Parse a repository spec into repositories.
///
/// Entries are separated by `,` or `;`. Each entry is `name=base_url[#set]`; a bare
/// `base_url[#set]` is accepted and named after its host. Blank entries are ignored.
pub fn parse_oai_repositories(spec: &str) -> Vec<OaiRepository> {
    spec.split(|c| c == ',' || c == ';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(parse_oai_repository)
        .collect()
}

/// Parse one `name=base_url[#set]` / `base_url[#set]` entry.
fn parse_oai_repository(entry: &str) -> Option<OaiRepository> {
    let (name, rest) = match entry.split_once('=') {
        Some((n, r)) if !n.trim().is_empty() => (n.trim().to_string(), r.trim()),
        _ => {
            let (url, _) = split_set(entry);
            let host = url::Url::parse(url).ok()?.host_str()?.to_string();
            (host, entry)
        }
    };
    let (url, set) = split_set(rest);
    if url.is_empty() {
        return None;
    }
    let mut repo = OaiRepository::new(name, url);
    if let Some(s) = set {
        repo = repo.with_set(s);
    }
    Some(repo)
}

/// Split `url#set` into its parts, trimming both.
fn split_set(s: &str) -> (&str, Option<String>) {
    match s.split_once('#') {
        Some((u, set)) if !set.trim().is_empty() => (u.trim(), Some(set.trim().to_string())),
        _ => (s.trim(), None),
    }
}

/// Append a query string to `base`, choosing `?` or `&` based on what `base` already carries.
fn join_query(base: &str, query: &str) -> String {
    let base = base.trim();
    if base.is_empty() {
        return format!("?{query}");
    }
    let sep = if base.ends_with('?') || base.ends_with('&') {
        ""
    } else if base.contains('?') {
        "&"
    } else {
        "?"
    };
    format!("{base}{sep}{query}")
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

    /// Records every URL it is asked for and returns a canned body — no network.
    struct StubTransport {
        body: Result<String, FetchError>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl StubTransport {
        fn ok(body: &str) -> Self {
            Self {
                body: Ok(body.to_string()),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                body: Err(FetchError::Http(msg.to_string())),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl HttpGet for StubTransport {
        async fn get(&self, url: &str) -> Result<String, FetchError> {
            self.seen.lock().unwrap().push(url.to_string());
            self.body.clone()
        }
    }

    #[test]
    fn query_url_encodes_parameters() {
        let url = arxiv_query_url("all:electron & photon", 25, 0);
        assert!(url.starts_with("http://export.arxiv.org/api/query?"));
        assert!(url.contains("search_query=all%3Aelectron+%26+photon"));
        assert!(url.contains("start=0"));
        assert!(url.contains("max_results=25"));
    }

    #[tokio::test]
    async fn fetch_wrapper_parses_through_a_stub_transport() {
        let stub = StubTransport::ok(TWO_ENTRIES);
        let links = fetch_arxiv_links(&stub, "all:electron", 10).await.unwrap();
        assert_eq!(
            links,
            vec![
                "https://arxiv.org/abs/2401.12345".to_string(),
                "https://arxiv.org/abs/2402.00001".to_string(),
            ]
        );
        let seen = stub.seen();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("search_query=all%3Aelectron"));
        assert!(seen[0].contains("max_results=10"));
    }

    #[tokio::test]
    async fn fetch_wrapper_propagates_transport_errors() {
        let stub = StubTransport::failing("connection refused");
        let err = fetch_arxiv_links(&stub, "all:x", 1).await.unwrap_err();
        assert!(matches!(err, FetchError::Http(_)));
        assert!(err.to_string().contains("connection refused"));
    }

    #[test]
    fn oai_list_records_url_carries_verb_prefix_and_bounds() {
        let repo = OaiRepository::new("arxiv", "http://export.arxiv.org/oai2")
            .with_set("cs:cs");
        let url = oai_list_records_url(&repo, Some("2024-01-01"), Some("2024-02-01"));
        assert!(url.starts_with("http://export.arxiv.org/oai2?"));
        assert!(url.contains("verb=ListRecords"));
        assert!(url.contains("metadataPrefix=oai_dc"));
        assert!(url.contains("set=cs%3Acs"));
        assert!(url.contains("from=2024-01-01"));
        assert!(url.contains("until=2024-02-01"));
    }

    #[test]
    fn oai_list_records_url_omits_empty_bounds_and_joins_existing_query() {
        let repo = OaiRepository::new("pmc", "https://example.org/oai?token=abc");
        let url = oai_list_records_url(&repo, None, Some("  "));
        assert!(url.starts_with("https://example.org/oai?token=abc&"));
        assert!(!url.contains("from="));
        assert!(!url.contains("until="));
        assert!(!url.contains("set="));
    }

    #[test]
    fn oai_urls_are_produced_for_every_repository_in_order() {
        let repos = vec![
            OaiRepository::new("a", "http://a.example/oai"),
            OaiRepository::new("b", "http://b.example/oai"),
        ];
        let urls = oai_list_records_urls(&repos, None, None);
        assert_eq!(urls.len(), 2);
        assert!(urls[0].starts_with("http://a.example/oai?"));
        assert!(urls[1].starts_with("http://b.example/oai?"));
        assert_eq!(default_oai_repositories().len(), 1);
    }

    #[test]
    fn parse_oai_repositories_handles_names_sets_and_bare_hosts() {
        let repos = parse_oai_repositories(
            "arxiv=http://export.arxiv.org/oai2#cs:cs; http://example.org/oai, ",
        );
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].name, "arxiv");
        assert_eq!(repos[0].base_url, "http://export.arxiv.org/oai2");
        assert_eq!(repos[0].set.as_deref(), Some("cs:cs"));
        assert_eq!(repos[1].name, "example.org");
        assert_eq!(repos[1].base_url, "http://example.org/oai");
        assert_eq!(repos[1].set, None);

        assert!(parse_oai_repositories("   ").is_empty());
    }
}
