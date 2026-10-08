//! Canonicalization dedupe and SSRF blocking (spec §8-§9).

use std::net::IpAddr;

use search_fetcher::canon::canonicalize;
use search_fetcher::ssrf::{is_blocked_ip, validate_url};

#[test]
fn canonicalize_dedupes_utm_params_and_fragments() {
    let with_junk = canonicalize("HTTP://Example.COM:80/path?id=7&utm_source=x&fbclid=z#frag").unwrap();
    let clean = canonicalize("http://example.com/path?id=7").unwrap();
    assert_eq!(with_junk, clean);
    assert!(!with_junk.contains("utm_source"));
    assert!(!with_junk.contains("fbclid"));
    assert!(!with_junk.contains('#'));
    assert!(!with_junk.contains(":80"));
}

#[test]
fn canonicalize_sorts_query_params() {
    let a = canonicalize("https://e.com/?b=2&a=1").unwrap();
    let b = canonicalize("https://e.com/?a=1&b=2").unwrap();
    assert_eq!(a, b);
    assert!(a.ends_with("?a=1&b=2"));
}

#[test]
fn canonicalize_strips_default_ports_only() {
    assert_eq!(canonicalize("https://e.com:443/").unwrap(), "https://e.com/");
    assert_eq!(canonicalize("http://e.com:80/").unwrap(), "http://e.com/");
    assert_eq!(canonicalize("http://e.com:8080/").unwrap(), "http://e.com:8080/");
}

#[test]
fn canonicalize_resolves_dot_segments_and_keeps_trailing_slash() {
    assert_eq!(canonicalize("http://e.com/a/b/../c").unwrap(), "http://e.com/a/c");
    // Spec does not declare trailing slashes insignificant.
    assert_eq!(canonicalize("http://e.com/a/").unwrap(), "http://e.com/a/");
}

#[test]
fn ssrf_blocks_loopback_metadata_and_private() {
    assert!(validate_url("http://127.0.0.1/").is_err());
    assert!(validate_url("http://127.0.0.1:8080/x").is_err());
    assert!(validate_url("http://169.254.169.254/latest/meta-data/").is_err());
    assert!(validate_url("http://[::1]/").is_err());
    assert!(validate_url("http://10.0.0.1/").is_err());
    assert!(validate_url("http://192.168.1.1/").is_err());
    assert!(validate_url("http://172.16.0.1/").is_err());
    assert!(validate_url("http://localhost/").is_err());
}

#[test]
fn ssrf_rejects_non_http_schemes() {
    assert!(validate_url("ftp://example.com/").is_err());
    assert!(validate_url("file:///etc/passwd").is_err());
    assert!(validate_url("gopher://example.com/").is_err());
}

#[test]
fn ssrf_allows_public_addresses() {
    assert!(validate_url("http://93.184.216.34/").is_ok());
    assert!(validate_url("https://example.com/some/path").is_ok());
}

#[test]
fn is_blocked_ip_covers_required_ranges_and_ipv4_mapped() {
    let b = |s: &str| is_blocked_ip(s.parse::<IpAddr>().unwrap());
    for s in [
        "127.0.0.1",
        "169.254.169.254",
        "10.0.0.1",
        "172.16.5.4",
        "192.168.0.1",
        "::1",
        "fe80::1",
        "fc00::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "224.0.0.1",
        "255.255.255.255",
    ] {
        assert!(b(s), "{s} must be blocked");
    }
    for s in ["93.184.216.34", "8.8.8.8", "2606:4700:4700::1111"] {
        assert!(!b(s), "{s} must be allowed");
    }
}
