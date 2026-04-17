//! Workspace and session state.
//!
//! Phase 0 keeps everything in RAM. The `Core` is `Sync`; every workspace
//! guards its mutable state behind a `Mutex`. Concurrent requests across
//! different workspaces run in parallel; requests on the same workspace
//! serialize (acceptable for Phase 0).

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use pf_graph::GraphStore;
use pf_protocol::WorkspaceId;
use pf_rules::Rule;

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

#[derive(Default)]
pub struct Core {
    workspaces: RwLock<HashMap<String, Workspace>>,
    next_id: Mutex<u64>,
}

impl Core {
    pub fn new() -> Self {
        Self::default()
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
