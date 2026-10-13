//! What happened when a model was asked — recorded so a failure is visible, not silent.
//!
//! The search path tries a chain of providers/models (free ones first). Without a record of the
//! attempts, a query that finally succeeded on the fourth model looks identical to one that
//! succeeded first time, and a query that failed looks like "the engine has no answer". Each
//! attempt carries the provider and model tried, whether it worked, and — when it did not — the
//! provider's own error text, so the reason is auditable from the response alone.

use serde::{Deserialize, Serialize};

/// One model attempt on the search path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmAttempt {
    /// Short provider label (e.g. `openrouter`, `crowbot`) — never a credential.
    pub provider: String,
    /// The model id asked (e.g. `google/gemma-4-31b-it:free`).
    pub model: String,
    /// Whether this attempt produced a usable completion.
    pub ok: bool,
    /// The provider's error, when `ok` is false.
    pub error: Option<String>,
}

impl LlmAttempt {
    /// A failed attempt.
    pub fn failed(provider: impl Into<String>, model: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            ok: false,
            error: Some(error.into()),
        }
    }

    /// A successful attempt.
    pub fn ok(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            ok: true,
            error: None,
        }
    }

    /// A one-line summary for a response trace: what was tried and how it went.
    pub fn summary(&self) -> String {
        match (&self.ok, &self.error) {
            (true, _) => format!("{} {} ok", self.provider, self.model),
            (false, Some(e)) => format!("{} {} failed ({e})", self.provider, self.model),
            (false, None) => format!("{} {} failed", self.provider, self.model),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_name_the_model_and_the_reason() {
        let ok = LlmAttempt::ok("openrouter", "google/gemma-4-31b-it:free");
        assert_eq!(ok.summary(), "openrouter google/gemma-4-31b-it:free ok");

        let bad = LlmAttempt::failed("openrouter", "google/gemma-4-26b-a4b-it:free", "HTTP 429");
        assert_eq!(
            bad.summary(),
            "openrouter google/gemma-4-26b-a4b-it:free failed (HTTP 429)"
        );
    }
}
