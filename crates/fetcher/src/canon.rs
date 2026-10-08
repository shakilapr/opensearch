//! URL canonicalization (spec §9, crawler §13). OWNER: fetcher agent.
//!
//! Canonical form: lowercase scheme + host, strip default ports, drop the
//! fragment, drop known tracking parameters, and sort the remaining query
//! parameters so that order-preserving duplicates collapse to one identity.
//! Dot-segment resolution is performed by the `url` crate during parsing.
//!
//! Trailing slashes are intentionally NOT stripped: the Stage-0 spec does not
//! say they are insignificant, and `/a` and `/a/` may be distinct resources.

use url::Url;

/// Query parameters that never affect resource identity.
const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "fbclid",
    "gclid",
];

/// Canonicalize a URL for stable identity: lowercase scheme/host, strip
/// default ports, remove fragments, sort/keep significant query params, drop
/// tracking params, resolve dot-segments. Returns the canonical string.
pub fn canonicalize(raw: &str) -> Result<String, url::ParseError> {
    let mut u = Url::parse(raw)?;

    // Fragments never identify a distinct resource.
    u.set_fragment(None);

    // Drop default ports.
    let default_port = match u.scheme() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    if default_port.is_some() && u.port() == default_port {
        let _ = u.set_port(None);
    }

    // Lowercase the host explicitly (the URL standard already does this for
    // special schemes, but be defensive for opaque/other schemes).
    if let Some(host) = u.host_str() {
        let lowered = host.to_ascii_lowercase();
        if lowered != host {
            let _ = u.set_host(Some(&lowered));
        }
    }

    // Filter + sort query parameters (stable identity under reordering).
    if u.query().is_some() {
        let mut kept: Vec<(String, String)> = u
            .query_pairs()
            .filter(|(k, _)| !TRACKING_PARAMS.contains(&k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        kept.sort();
        if kept.is_empty() {
            u.set_query(None);
        } else {
            let mut qs = u.query_pairs_mut();
            qs.clear();
            for (k, v) in &kept {
                qs.append_pair(k, v);
            }
        }
    }

    Ok(u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_fragment_and_utm() {
        let c = canonicalize("http://Example.COM/p?utm_source=x&id=7&gclid=z#section").unwrap();
        assert_eq!(c, "http://example.com/p?id=7");
    }

    #[test]
    fn sorts_query() {
        let a = canonicalize("https://e.com/?b=2&a=1").unwrap();
        let b = canonicalize("https://e.com/?a=1&b=2").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, "https://e.com/?a=1&b=2");
    }

    #[test]
    fn resolves_dot_segments() {
        assert_eq!(
            canonicalize("http://e.com/a/b/../c").unwrap(),
            "http://e.com/a/c"
        );
    }
}
