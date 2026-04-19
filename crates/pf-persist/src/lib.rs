//! Persistence abstraction.
//!
//! Phase 0 ships an in-memory backend only. The trait shape is already
//! compatible with a future RocksDB / SQLite backend that will land in
//! Phase 1 — no caller needs to change.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PersistError {
    #[error("key not found: {0}")]
    NotFound(String),
    #[error("backend error: {0}")]
    Backend(String),
}

/// A minimal typed KV. Keys are `(namespace, key)`; values are opaque bytes.
/// Content-addressed storage, a second namespace for the event journal, and
/// cache layers are added in Phase 1.
pub trait Store: Send + Sync {
    fn put(&self, namespace: &str, key: &str, value: Vec<u8>) -> Result<(), PersistError>;
    fn get(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>, PersistError>;
    fn delete(&self, namespace: &str, key: &str) -> Result<(), PersistError>;
    fn list(&self, namespace: &str) -> Result<Vec<String>, PersistError>;
}

#[derive(Debug, Default)]
pub struct MemStore {
    inner: Mutex<HashMap<String, HashMap<String, Vec<u8>>>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, HashMap<String, Vec<u8>>>> {
        self.inner.lock().expect("MemStore mutex poisoned")
    }
}

impl Store for MemStore {
    fn put(&self, namespace: &str, key: &str, value: Vec<u8>) -> Result<(), PersistError> {
        let mut g = self.lock();
        g.entry(namespace.to_string())
            .or_default()
            .insert(key.to_string(), value);
        Ok(())
    }

    fn get(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>, PersistError> {
        let g = self.lock();
        Ok(g.get(namespace).and_then(|ns| ns.get(key).cloned()))
    }

    fn delete(&self, namespace: &str, key: &str) -> Result<(), PersistError> {
        let mut g = self.lock();
        if let Some(ns) = g.get_mut(namespace) {
            ns.remove(key);
        }
        Ok(())
    }

    fn list(&self, namespace: &str) -> Result<Vec<String>, PersistError> {
        let g = self.lock();
        Ok(g.get(namespace)
            .map(|ns| ns.keys().cloned().collect())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_round_trip() {
        let s = MemStore::new();
        s.put("ns", "k", b"v".to_vec()).unwrap();
        assert_eq!(s.get("ns", "k").unwrap().as_deref(), Some(&b"v"[..]));
        s.delete("ns", "k").unwrap();
        assert_eq!(s.get("ns", "k").unwrap(), None);
    }
}
