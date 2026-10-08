//! SSRF guard (spec §8). OWNER: fetcher agent.
//!
//! Every URL and every redirect target is validated. We check the literal host,
//! and — critically — resolve DNS and re-check **every** resolved IP so a
//! hostname cannot rebind to a loopback/private address after the check.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use search_core::{AppError, AppResult};
use url::Url;

/// Validate a URL without touching the network: scheme allow-list plus a
/// literal-IP check (and obvious internal hostnames). Use
/// [`validate_url_resolved`] to also resolve and re-check DNS.
pub fn validate_url(raw: &str) -> AppResult<()> {
    let u = Url::parse(raw).map_err(|e| AppError::SsrfBlocked(format!("invalid url {raw:?}: {e}")))?;

    match u.scheme() {
        "http" | "https" => {}
        other => {
            return Err(AppError::SsrfBlocked(format!(
                "scheme {other:?} is not allowed (http/https only)"
            )))
        }
    }

    let host = u
        .host_str()
        .ok_or_else(|| AppError::SsrfBlocked("url has no host".into()))?;

    // Explicit internal hostnames.
    let lh = host.to_ascii_lowercase();
    if lh == "localhost"
        || lh.ends_with(".localhost")
        || lh == "metadata.google.internal"
        || lh == "metadata"
    {
        return Err(AppError::SsrfBlocked(format!("blocked host {host:?}")));
    }

    // Literal IP (strip IPv6 brackets).
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(AppError::SsrfBlocked(format!("blocked ip {ip}")));
        }
    }

    Ok(())
}

/// Validate a URL end-to-end: parse, literal check, then resolve the host and
/// re-validate **every** resolved address. Returns the resolved addresses.
pub async fn validate_url_resolved(raw: &str) -> AppResult<Vec<IpAddr>> {
    validate_url(raw)?;
    let u = Url::parse(raw).map_err(|e| AppError::SsrfBlocked(format!("invalid url {raw:?}: {e}")))?;
    let host = u
        .host_str()
        .ok_or_else(|| AppError::SsrfBlocked("url has no host".into()))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();

    // Already a literal IP: it passed the check above.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }

    let port = u.port_or_known_default().unwrap_or(80);
    let addrs = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| AppError::Network(format!("dns lookup for {host:?} failed: {e}")))?;

    let mut ips = Vec::new();
    for a in addrs {
        let ip = a.ip();
        if is_blocked_ip(ip) {
            return Err(AppError::SsrfBlocked(format!(
                "host {host:?} resolves to blocked ip {ip}"
            )));
        }
        ips.push(ip);
    }
    if ips.is_empty() {
        return Err(AppError::Network(format!(
            "dns lookup for {host:?} returned no addresses"
        )));
    }
    Ok(ips)
}

/// Whether a literal IP address is in the block list.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(v: Ipv4Addr) -> bool {
    let o = v.octets();
    v.is_loopback()               // 127.0.0.0/8
        || v.is_private()          // 10/8, 172.16/12, 192.168/16
        || v.is_link_local()       // 169.254.0.0/16 (cloud metadata)
        || v.is_multicast()        // 224.0.0.0/4
        || v.is_broadcast()        // 255.255.255.255
        || v.is_unspecified()      // 0.0.0.0
        || o[0] == 0               // 0.0.0.0/8
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
        || v.is_documentation()    // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // 198.18.0.0/15 benchmarking
        || (o[0] == 240 && (o[1] & 0xf0) == 0) // 240.0.0.0/4 reserved
}

fn is_blocked_v6(v: Ipv6Addr) -> bool {
    // IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible (::a.b.c.d) addresses
    // are re-checked as their embedded IPv4 address.
    if let Some(v4) = v.to_ipv4() {
        if is_blocked_v4(v4) {
            return true;
        }
    }
    v.is_loopback()                        // ::1
        || v.is_unspecified()              // ::
        || v.is_multicast()                // ff00::/8
        || (v.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        || (v.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> bool {
        is_blocked_ip(s.parse::<IpAddr>().unwrap())
    }

    #[test]
    fn blocks_the_required_ranges() {
        for s in [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.5.4",
            "172.31.255.1",
            "192.168.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(b(s), "{s} should be blocked");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for s in ["93.184.216.34", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(!b(s), "{s} should be allowed");
        }
    }
}
