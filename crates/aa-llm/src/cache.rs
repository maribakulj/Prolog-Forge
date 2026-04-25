//! Content-addressed response cache.
//!
//! The cache key is a hash of `(system, user, schema_id, max_tokens,
//! temperature, provider_name)`. Identical inputs under the same provider
//! return byte-identical responses — a prerequisite for reproducible
//! provenance. In-memory in Phase 1.2; promoted to the persistence layer
//! once `aa-persist` grows a disk backend.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use crate::provider::{LlmRequest, LlmResponse};

#[derive(Default)]
pub struct ResponseCache {
    inner: Mutex<HashMap<u64, LlmResponse>>,
}

impl ResponseCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, provider: &str, req: &LlmRequest) -> Option<LlmResponse> {
        let k = key(provider, req);
        self.inner.lock().unwrap().get(&k).cloned()
    }

    pub fn put(&self, provider: &str, req: &LlmRequest, resp: LlmResponse) {
        let k = key(provider, req);
        self.inner.lock().unwrap().insert(k, resp);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn key(provider: &str, req: &LlmRequest) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    provider.hash(&mut h);
    req.system.hash(&mut h);
    req.user.hash(&mut h);
    req.schema_id.hash(&mut h);
    req.max_tokens.hash(&mut h);
    req.temperature.to_bits().hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let c = ResponseCache::new();
        let r = LlmRequest {
            system: "s".into(),
            user: "u".into(),
            schema_id: "x".into(),
            max_tokens: 10,
            temperature: 0.0,
        };
        assert!(c.get("p", &r).is_none());
        c.put(
            "p",
            &r,
            LlmResponse {
                text: "hi".into(),
                tokens_in: 1,
                tokens_out: 1,
                provider: "p".into(),
            },
        );
        assert_eq!(c.get("p", &r).unwrap().text, "hi");
    }
}
