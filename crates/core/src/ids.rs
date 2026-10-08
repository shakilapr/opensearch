//! The five semantic IDs plus operational keys.
//!
//! These are the ONLY identifiers the system invents. Do not add parallel
//! identity spaces. `OpKey` is the ingestion idempotency key
//! `(resource_id, fetch_observation, representation_version)` — an operational
//! key, not a sixth semantic identity.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }
    };
}

id_type!(DomainId, "Hostname, e.g. `arxiv.org`.");
id_type!(SourceId, "Logical collection targeted by routing, e.g. `docs.python.org/asyncio`.");
id_type!(ResourceId, "Fetchable address: URL / sitemap / feed / PDF. Stable hash of canonical URL.");
id_type!(WorkId, "Logical intellectual object spanning preprint+publisher.");
id_type!(ContentId, "Deduplicated payload: hash of exact bytes.");
id_type!(DocVerId, "One immutable document version: `{resource_id}:{version_no}`.");
id_type!(DocId, "Stable document identity used as the Tantivy delete term.");
id_type!(ManifestId, "Identifier of a published index manifest generation.");
id_type!(OpKey, "Ingestion idempotency key: `{resource_id}|{obs_seq}|{repr_version}`.");

impl ResourceId {
    /// Deterministic resource id from a canonical URL. Stable across restarts so
    /// re-discovery of the same URL never creates a duplicate resource.
    pub fn from_canonical_url(canonical_url: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(canonical_url.as_bytes());
        let digest = h.finalize();
        Self(format!("res_{}", hex::encode(&digest[..16])))
    }
}

impl ContentId {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        let digest = h.finalize();
        Self(format!("cnt_{}", hex::encode(&digest[..16])))
    }
}

impl DomainId {
    pub fn from_host(host: &str) -> Self {
        Self(host.to_ascii_lowercase())
    }
}

impl DocVerId {
    pub fn from_parts(resource_id: &ResourceId, version_no: u64) -> Self {
        Self(format!("{}:{}", resource_id.as_str(), version_no))
    }
}

impl OpKey {
    pub fn from_parts(resource_id: &ResourceId, obs_seq: i64, repr_version: u64) -> Self {
        Self(format!(
            "{}|{}|{}",
            resource_id.as_str(),
            obs_seq,
            repr_version
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_id_is_stable() {
        let a = ResourceId::from_canonical_url("https://arxiv.org/abs/2401.00001");
        let b = ResourceId::from_canonical_url("https://arxiv.org/abs/2401.00001");
        assert_eq!(a, b);
        let c = ResourceId::from_canonical_url("https://arxiv.org/abs/2401.00002");
        assert_ne!(a, c);
    }

    #[test]
    fn content_id_is_deterministic() {
        assert_eq!(ContentId::from_bytes(b"hello"), ContentId::from_bytes(b"hello"));
        assert_ne!(ContentId::from_bytes(b"hello"), ContentId::from_bytes(b"world"));
    }

    #[test]
    fn docver_format() {
        let r = ResourceId::new("res_x");
        assert_eq!(DocVerId::from_parts(&r, 3).as_str(), "res_x:3");
    }
}
