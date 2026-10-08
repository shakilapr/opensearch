//! Robots Exclusion Protocol (RFC 9309) (spec §9). OWNER: fetcher agent.
//!
//! Failure semantics are part of the contract and are tested explicitly:
//!
//! * a **successfully fetched** file's rules are obeyed (longest-match-wins,
//!   Allow beats Disallow on ties);
//! * **4xx / unavailable** => [`RobotsStatus::Unavailable`] => MAY crawl;
//! * **5xx / network failure** => [`RobotsStatus::Unreachable`] => complete
//!   disallow.

use regex::Regex;

/// RFC 9309 §2.5: a crawler MUST parse at least 500 KiB of a robots file.
pub const MIN_ROBOTS_BYTES: u64 = 512 * 1024;

/// The outcome of evaluating robots for a URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobotsVerdict {
    Allowed,
    Disallowed,
}

impl RobotsVerdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, RobotsVerdict::Allowed)
    }
}

/// Cached robots state for one origin.
#[derive(Debug, Clone)]
pub struct RobotsPolicy {
    pub domain_id: String,
    pub status: RobotsStatus,
    pub fetched_at: i64,
    pub rules: Vec<RobotsRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobotsStatus {
    /// Fetched successfully; rules apply.
    Ok,
    /// 4xx: MAY access any resource.
    Unavailable,
    /// 5xx or network error: MUST assume complete disallow.
    Unreachable,
}

impl RobotsStatus {
    /// Map a robots.txt HTTP status onto RFC 9309 failure semantics.
    pub fn from_http_status(code: u16) -> RobotsStatus {
        match code {
            200..=299 => RobotsStatus::Ok,
            400..=499 => RobotsStatus::Unavailable,
            _ => RobotsStatus::Unreachable,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RobotsRule {
    pub agent: String,
    pub allow: bool,
    pub path: String,
}

impl RobotsPolicy {
    pub fn empty(domain_id: impl Into<String>, fetched_at: i64) -> Self {
        Self {
            domain_id: domain_id.into(),
            status: RobotsStatus::Ok,
            fetched_at,
            rules: Vec::new(),
        }
    }

    /// 4xx: crawler MAY access any resource.
    pub fn unavailable(domain_id: impl Into<String>, fetched_at: i64) -> Self {
        Self {
            domain_id: domain_id.into(),
            status: RobotsStatus::Unavailable,
            fetched_at,
            rules: Vec::new(),
        }
    }

    /// 5xx or network error: crawler MUST assume complete disallow.
    pub fn unreachable(domain_id: impl Into<String>, fetched_at: i64) -> Self {
        Self {
            domain_id: domain_id.into(),
            status: RobotsStatus::Unreachable,
            fetched_at,
            rules: Vec::new(),
        }
    }

    pub fn is_allowed(&self, path: &str) -> bool {
        evaluate(self, path).is_allowed()
    }
}

/// A parsed group: one or more user-agent tokens followed by their rules.
#[derive(Debug, Default, Clone)]
struct Group {
    agents: Vec<String>,
    rules: Vec<(bool, String)>, // (is_allow, path)
}

/// Parse a robots.txt body and select the rules that apply to `user_agent`.
///
/// Group semantics: consecutive `User-agent` lines prefix a group; a run of
/// rules belongs to the group; a blank line or a new `User-agent` line after
/// rules ends the group. Comment (`#`) and unknown fields (`sitemap`,
/// `crawl-delay`, ...) are ignored.
pub fn parse(body: &str, user_agent: &str, fetched_at: i64) -> RobotsPolicy {
    let groups = parse_groups(body);
    let (matched_agent, rules) = match select_group(&groups, user_agent) {
        Some((g, token)) => (
            token,
            g.rules
                .iter()
                .map(|(allow, path)| RobotsRule {
                    agent: String::new(),
                    allow: *allow,
                    path: path.clone(),
                })
                .collect::<Vec<_>>(),
        ),
        None => (String::new(), Vec::new()),
    };

    let rules = rules
        .into_iter()
        .map(|r| RobotsRule {
            agent: matched_agent.clone(),
            ..r
        })
        .collect();

    RobotsPolicy {
        domain_id: String::new(),
        status: RobotsStatus::Ok,
        fetched_at,
        rules,
    }
}

/// Parse a robots.txt body while never reading more than `max_bytes`; rules
/// present before the truncation point are still enforced (we never fail open
/// by turning a parse failure into an all-permissive policy).
pub fn parse_capped(body: &str, user_agent: &str, fetched_at: i64, max_bytes: u64) -> RobotsPolicy {
    let cap = max_bytes.max(MIN_ROBOTS_BYTES) as usize;
    if body.len() <= cap {
        return parse(body, user_agent, fetched_at);
    }
    // Truncate on a char boundary so the parser sees valid UTF-8.
    let mut end = cap;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    parse(&body[..end], user_agent, fetched_at)
}

fn parse_groups(body: &str) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut current: Option<Group> = None;

    for raw in body.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            if let Some(g) = current.take() {
                if !g.agents.is_empty() {
                    groups.push(g);
                }
            }
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => continue,
        };

        match field.as_str() {
            "user-agent" | "useragent" => {
                let start_new = matches!(&current, Some(g) if !g.rules.is_empty());
                if start_new {
                    if let Some(g) = current.take() {
                        if !g.agents.is_empty() {
                            groups.push(g);
                        }
                    }
                }
                current.get_or_insert_with(Group::default);
                current
                    .as_mut()
                    .unwrap()
                    .agents
                    .push(value.to_ascii_lowercase());
            }
            "allow" | "disallow" => {
                // Rules before any User-agent have no group; ignore them.
                if let Some(g) = current.as_mut() {
                    if !g.agents.is_empty() {
                        g.rules.push((field == "allow", value));
                    }
                }
            }
            _ => {} // sitemap, crawl-delay, host, ... ignored
        }
    }

    if let Some(g) = current.take() {
        if !g.agents.is_empty() {
            groups.push(g);
        }
    }
    groups
}

/// Pick the most specific matching group: the longest user-agent product token
/// that is a (case-insensitive) substring of the configured UA wins; `*` is the
/// fallback. Returns the group and the matched token.
fn select_group<'a>(groups: &'a [Group], user_agent: &str) -> Option<(&'a Group, String)> {
    let ua = user_agent.to_ascii_lowercase();
    let mut best: Option<(usize, &'a Group, String)> = None;

    for g in groups {
        for agent in &g.agents {
            if agent.is_empty() || agent == "*" {
                continue;
            }
            if ua.contains(agent.as_str()) {
                let spec = agent.len();
                if best.as_ref().map_or(true, |(s, _, _)| spec > *s) {
                    best = Some((spec, g, agent.clone()));
                }
            }
        }
    }
    if let Some((_, g, token)) = best {
        return Some((g, token));
    }
    groups
        .iter()
        .find(|g| g.agents.iter().any(|a| a == "*"))
        .map(|g| (g, "*".to_string()))
}

/// Pure evaluation of a policy against a request path. RFC 9309 semantics.
pub fn evaluate(policy: &RobotsPolicy, path: &str) -> RobotsVerdict {
    match policy.status {
        RobotsStatus::Unreachable => return RobotsVerdict::Disallowed,
        RobotsStatus::Unavailable => return RobotsVerdict::Allowed,
        RobotsStatus::Ok => {}
    }

    // Longest matching pattern wins; Allow beats Disallow on equal length.
    let mut best: Option<(usize, bool)> = None; // (pattern_len, is_allow)
    for rule in &policy.rules {
        let pattern = rule.path.trim();
        if pattern.is_empty() {
            continue; // an empty Disallow/Allow imposes no restriction
        }
        if !rule_matches(pattern, path) {
            continue;
        }
        let len = pattern.len();
        match best {
            Some((bl, ba)) => {
                if len > bl || (len == bl && rule.allow && !ba) {
                    best = Some((len, rule.allow));
                }
            }
            None => best = Some((len, rule.allow)),
        }
    }

    match best {
        Some((_, true)) | None => RobotsVerdict::Allowed,
        Some((_, false)) => RobotsVerdict::Disallowed,
    }
}

/// Does `pattern` (RFC 9309 path pattern, may contain `*` and a trailing `$`)
/// match `path`? Without `$` this is a prefix match on the pattern.
fn rule_matches(pattern: &str, path: &str) -> bool {
    match compile_rule(pattern) {
        Some(re) => re.is_match(path),
        None => false,
    }
}

fn compile_rule(pattern: &str) -> Option<Regex> {
    let anchored_end = pattern.ends_with('$');
    let core = if anchored_end {
        &pattern[..pattern.len() - 1]
    } else {
        pattern
    };
    let mut re = String::with_capacity(core.len() + 4);
    re.push('^');
    for ch in core.chars() {
        if ch == '*' {
            re.push_str(".*");
        } else {
            re.push_str(&regex::escape(&ch.to_string()));
        }
    }
    if anchored_end {
        re.push('$');
    }
    Regex::new(&re).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        assert_eq!(RobotsStatus::from_http_status(200), RobotsStatus::Ok);
        assert_eq!(RobotsStatus::from_http_status(404), RobotsStatus::Unavailable);
        assert_eq!(RobotsStatus::from_http_status(403), RobotsStatus::Unavailable);
        assert_eq!(RobotsStatus::from_http_status(500), RobotsStatus::Unreachable);
        assert_eq!(RobotsStatus::from_http_status(503), RobotsStatus::Unreachable);
    }

    #[test]
    fn truncation_still_enforces_early_rules() {
        let mut body = String::from("User-agent: *\nDisallow: /secret\n");
        body.push_str(&"# pad\n".repeat(200_000));
        let p = parse_capped(&body, "bot", 0, MIN_ROBOTS_BYTES);
        assert_eq!(evaluate(&p, "/secret/x"), RobotsVerdict::Disallowed);
        assert!(MIN_ROBOTS_BYTES >= 512_000, "RFC 9309 requires >= 500 KiB");
    }
}
