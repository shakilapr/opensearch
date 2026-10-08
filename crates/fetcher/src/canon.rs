//! URL canonicalization (spec §9, crawler §13). OWNER: fetcher agent.

/// Canonicalize a URL for stable identity: lowercase scheme/host, strip
/// default ports, remove fragments, sort/keep significant query params, drop
/// tracking params. Returns the canonical string.
pub fn canonicalize(raw: &str) -> Result<String, url::ParseError> {
    let mut u = url::Url::parse(raw)?;
    u.set_fragment(None);
    // Remove default ports.
    if (u.scheme() == "http" && u.port() == Some(80)) || (u.scheme() == "https" && u.port() == Some(443)) {
        let _ = u.set_port(None);
    }
    // Drop common tracking params.
    let drop = ["utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content", "fbclid", "gclid"];
    if u.query().is_some() {
        let kept: Vec<(String, String)> = u
            .query_pairs()
            .filter(|(k, _)| !drop.contains(&k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if kept.is_empty() {
            u.set_query(None);
        } else {
            let mut qs = u.query_pairs_mut();
            qs.clear();
            for (k, v) in kept {
                qs.append_pair(&k, &v);
            }
        }
    }
    Ok(u.to_string())
}
