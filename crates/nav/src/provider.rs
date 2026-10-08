//! Provider adapters (engine §19, engine-arch §12).
//!
//! A [`ProviderPolicy`] wraps a [`Provider`] + an injected [`ModelClient`]. The model sees
//! **only** structured state and allowed action ids — never a raw URL, never an arbitrary
//! target string. No API keys live here; the client is injected so the default build and all
//! tests compile and run without credentials.

use std::sync::Arc;

use crate::error::NavError;
use crate::policy::{PolicyTier, SearchPolicy};
use crate::state::{Action, SearchAction, SearchState};

/// Which backend a [`ProviderPolicy`] targets. `Local` is the small on-device model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    OpenAI,
    Claude,
    DeepSeek,
    Glm,
    Local,
}

impl Provider {
    /// Stable lowercase tag.
    pub fn tag(&self) -> &'static str {
        match self {
            Provider::OpenAI => "openai",
            Provider::Claude => "claude",
            Provider::DeepSeek => "deepseek",
            Provider::Glm => "glm",
            Provider::Local => "local",
        }
    }
}

/// The one method a provider adapter needs. Implemented by the real HTTP clients and by the
/// hermetic fakes in tests.
pub trait ModelClient: Send + Sync {
    fn complete(&self, prompt: &str) -> Result<String, NavError>;
}

/// Turns structured state + allowed action ids into a prompt, calls the client, and maps the
/// returned id back to an offered action.
pub struct ProviderPolicy {
    pub provider: Provider,
    pub client: Arc<dyn ModelClient>,
}

impl std::fmt::Debug for ProviderPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderPolicy")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl ProviderPolicy {
    pub fn new(provider: Provider, client: Arc<dyn ModelClient>) -> Self {
        Self { provider, client }
    }
}

/// Redact URL-like tokens. The prompt is built so a URL cannot leak: any whitespace-separated
/// token containing `://` or starting with `www.` becomes `[url]`.
fn redact(s: &str) -> String {
    s.split_whitespace()
        .map(|t| {
            if t.contains("://") || t.starts_with("www.") {
                "[url]"
            } else {
                t
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the model prompt and the list of actions that received an id.
///
/// The prompt carries only: a redacted query, numeric state counters, and the allowed actions
/// as `An kind=.. gain=.. cost=..`. No target strings, no URLs.
pub fn build_prompt(state: &SearchState, actions: &[SearchAction]) -> (String, Vec<SearchAction>) {
    let allowed: Vec<SearchAction> = actions
        .iter()
        .filter(|a| a.admissible)
        .cloned()
        .collect();

    let mut p = String::new();
    p.push_str("Select exactly one action id that best advances the search.\n");
    p.push_str("Reply with a single id (A0, A1, ...) or NONE. No prose, no URLs.\n\n");
    p.push_str("state:\n");
    p.push_str(&format!("  query: {}\n", redact(&state.query)));
    p.push_str(&format!("  hops: {}\n", state.hops));
    p.push_str(&format!("  visited_docs: {}\n", state.visited_docs.len()));
    p.push_str(&format!("  results: {}\n", state.results.len()));
    p.push_str("actions:\n");
    for (i, a) in allowed.iter().enumerate() {
        p.push_str(&format!(
            "  A{} kind={} gain={:.2} cost={:.2}\n",
            i,
            a.action.slug(),
            a.expected_gain,
            a.cost
        ));
    }
    p.push_str("answer:");
    (p, allowed)
}

/// Extract the first `A<digits>` id from a model reply. Case-insensitive.
fn parse_id(reply: &str) -> Option<usize> {
    let bytes = reply.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'A' || bytes[i] == b'a' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                if let Ok(n) = reply[start..j].parse::<usize>() {
                    return Some(n);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

impl SearchPolicy for ProviderPolicy {
    fn tier(&self) -> PolicyTier {
        match self.provider {
            Provider::Local => PolicyTier::LocalModel,
            _ => PolicyTier::Llm,
        }
    }

    fn choose(&self, state: &SearchState, actions: &[SearchAction]) -> Option<SearchAction> {
        let (prompt, allowed) = build_prompt(state, actions);
        let reply = self.client.complete(&prompt).ok()?;
        let trimmed = reply.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
            return None;
        }
        let idx = parse_id(trimmed)?;
        let picked = allowed.get(idx)?.clone();
        // Revalidate legality (engine-arch §12.2): the action must still be admissible and
        // not already seen.
        if picked.admissible && !state.seen(&picked.action) {
            Some(picked)
        } else {
            None
        }
    }
}

/// Convenience: pick an action by id from a structured prompt (used by fakes/tests).
#[allow(dead_code)]
pub(crate) fn action_for_id(allowed: &[SearchAction], id: &str) -> Option<Action> {
    let idx = parse_id(id)?;
    allowed.get(idx).map(|a| a.action.clone())
}
