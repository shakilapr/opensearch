//! RFC 9309 robots semantics (spec §9): failure modes, longest-match,
//! Allow-beats-Disallow, and user-agent specificity.

use search_fetcher::robots::{evaluate, parse, RobotsPolicy, RobotsStatus, RobotsVerdict};

fn verdict(body: &str, ua: &str, path: &str) -> RobotsVerdict {
    evaluate(&parse(body, ua, 0), path)
}

#[test]
fn robots_4xx_is_unavailable_and_permits_crawl() {
    let status = RobotsStatus::from_http_status(404);
    assert_eq!(status, RobotsStatus::Unavailable);
    let policy = RobotsPolicy {
        domain_id: "d".into(),
        status,
        fetched_at: 0,
        rules: vec![],
    };
    assert_eq!(evaluate(&policy, "/anything"), RobotsVerdict::Allowed);
    assert_eq!(evaluate(&policy, "/"), RobotsVerdict::Allowed);
}

#[test]
fn robots_5xx_is_unreachable_and_forbids_everything() {
    let status = RobotsStatus::from_http_status(503);
    assert_eq!(status, RobotsStatus::Unreachable);
    let policy = RobotsPolicy::unreachable("d", 0);
    assert_eq!(evaluate(&policy, "/"), RobotsVerdict::Disallowed);
    assert_eq!(evaluate(&policy, "/anything/else"), RobotsVerdict::Disallowed);
}

#[test]
fn network_failure_maps_to_unreachable_and_forbids() {
    // The fetch layer maps DNS/connect failures onto Unreachable.
    let policy = RobotsPolicy::unreachable("d", 0);
    assert_eq!(evaluate(&policy, "/x"), RobotsVerdict::Disallowed);
}

#[test]
fn allow_overrides_disallow_longest_match_wins() {
    // Most specific (longest) rule wins.
    let body = "User-agent: *\nDisallow: /private/\nAllow: /private/public/\n";
    assert_eq!(verdict(body, "bot", "/private/x"), RobotsVerdict::Disallowed);
    assert_eq!(verdict(body, "bot", "/private/public/x"), RobotsVerdict::Allowed);

    // Equal-length tie: Allow wins.
    let tie = "User-agent: *\nDisallow: /a\nAllow: /a\n";
    assert_eq!(verdict(tie, "bot", "/a"), RobotsVerdict::Allowed);

    // Longer Disallow beats a shorter Allow.
    let longer = "User-agent: *\nAllow: /a\nDisallow: /a/b\n";
    assert_eq!(verdict(longer, "bot", "/a/b/c"), RobotsVerdict::Disallowed);
    assert_eq!(verdict(longer, "bot", "/a/other"), RobotsVerdict::Allowed);
}

#[test]
fn user_agent_specificity_longest_token_wins() {
    let body = "User-agent: *\nDisallow: /\n\nUser-agent: agentnativesearch\nDisallow: /secret\n";
    // Specific group: allowed except /secret.
    assert_eq!(verdict(body, "AgentNativeSearchBot/0.1", "/public"), RobotsVerdict::Allowed);
    assert_eq!(verdict(body, "AgentNativeSearchBot/0.1", "/secret/x"), RobotsVerdict::Disallowed);
    // Unrelated UA falls back to ``*`` (disallow all).
    assert_eq!(verdict(body, "SomeOtherBot", "/public"), RobotsVerdict::Disallowed);

    // Among several matching tokens, the longest one wins.
    let body2 = "User-agent: search\nDisallow: /a\n\nUser-agent: agentnativesearch\nDisallow: /b\n";
    assert_eq!(verdict(body2, "AgentNativeSearchBot", "/a"), RobotsVerdict::Allowed);
    assert_eq!(verdict(body2, "AgentNativeSearchBot", "/b"), RobotsVerdict::Disallowed);
}

#[test]
fn empty_disallow_allows_all_and_wildcards_work() {
    assert_eq!(verdict("User-agent: *\nDisallow:\n", "bot", "/x"), RobotsVerdict::Allowed);

    let body = "User-agent: *\nDisallow: /*.pdf$\nDisallow: /tmp/*\n";
    assert_eq!(verdict(body, "bot", "/a/b/file.pdf"), RobotsVerdict::Disallowed);
    assert_eq!(verdict(body, "bot", "/a/b/file.pdfx"), RobotsVerdict::Allowed);
    assert_eq!(verdict(body, "bot", "/tmp/anything/here"), RobotsVerdict::Disallowed);
}

#[test]
fn no_rules_allows() {
    assert_eq!(verdict("", "bot", "/x"), RobotsVerdict::Allowed);
    assert_eq!(verdict("# only a comment\n", "bot", "/x"), RobotsVerdict::Allowed);
}
