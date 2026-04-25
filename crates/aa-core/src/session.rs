//! Workspace and session state.
//!
//! Phase 0 keeps everything in RAM. The `Core` is `Sync`; every workspace
//! guards its mutable state behind a `Mutex`. Concurrent requests across
//! different workspaces run in parallel; requests on the same workspace
//! serialize (acceptable for Phase 0).

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use aa_graph::GraphStore;
use aa_llm::{LlmProvider, MockProvider, ResponseCache};
use aa_protocol::WorkspaceId;
use aa_rules::Rule;

pub struct Workspace {
    pub id: WorkspaceId,
    pub root: String,
    pub state: Mutex<WorkspaceState>,
}

#[derive(Default)]
pub struct WorkspaceState {
    pub graph: GraphStore,
    pub rules: Vec<Rule>,
}

pub struct Core {
    workspaces: RwLock<HashMap<String, Workspace>>,
    next_id: Mutex<u64>,
    pub llm_provider: Box<dyn LlmProvider>,
    pub llm_cache: ResponseCache,
    /// Persistent rust-analyzer sessions keyed by workspace root.
    /// Amortises the indexing cost across successive typed-rename
    /// calls (see `crates/aa-core/src/ra_pool.rs`).
    pub ra_pool: crate::ra_pool::RaSessionPool,
}

impl Default for Core {
    fn default() -> Self {
        Self {
            workspaces: RwLock::new(HashMap::new()),
            next_id: Mutex::new(0),
            llm_provider: Box::new(MockProvider),
            llm_cache: ResponseCache::new(),
            ra_pool: crate::ra_pool::RaSessionPool::new(),
        }
    }
}

impl Core {
    pub fn new() -> Self {
        Self::default()
    }

    /// Swap the LLM provider (used by the daemon to inject a real backend
    /// when available; tests and CI default to `MockProvider`).
    pub fn with_llm_provider(mut self, p: Box<dyn LlmProvider>) -> Self {
        self.llm_provider = p;
        self
    }

    pub fn open(&self, root: String) -> WorkspaceId {
        let mut next = self.next_id.lock().unwrap();
        *next += 1;
        let id = WorkspaceId(format!("ws-{}", *next));
        let ws = Workspace {
            id: id.clone(),
            root,
            state: Mutex::new(WorkspaceState::default()),
        };
        self.workspaces.write().unwrap().insert(id.0.clone(), ws);
        id
    }

    /// Run `f` against the mutable state of workspace `id`. Returns
    /// `Err("unknown workspace")` if the id is not found.
    pub fn with_workspace<R>(
        &self,
        id: &WorkspaceId,
        f: impl FnOnce(&Workspace, &mut WorkspaceState) -> R,
    ) -> Result<R, &'static str> {
        let map = self.workspaces.read().unwrap();
        let ws = map.get(&id.0).ok_or("unknown workspace")?;
        let mut state = ws.state.lock().unwrap();
        Ok(f(ws, &mut state))
    }
}
