//! LSP message shapes we actually use.
//!
//! We track only the fields the rename flow touches, with `#[serde(default)]`
//! on everything so unexpected absence never blows up the parser. The LSP
//! spec defines dozens more fields; `#[serde(default)]` + flat structs keep
//! us resilient to the real server sending more than we need.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `file:///abs/path`. We build these from `Path`s via `DocumentUri::from_path`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocumentUri(pub String);

impl DocumentUri {
    pub fn from_path(p: &std::path::Path) -> Self {
        let s = p.to_string_lossy();
        // Minimal file:// URI: no percent-encoding for paths with spaces etc.
        // rust-analyzer accepts the unencoded form for local paths on Linux
        // (it uses the url crate internally which handles both). For a wider
        // portability story, depend on the `url` crate — out of scope here.
        if s.starts_with('/') {
            DocumentUri(format!("file://{s}"))
        } else {
            DocumentUri(format!("file:///{s}"))
        }
    }
}

/// Zero-indexed. LSP `Position.line` and `Position.character` are both 0-based.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEdit {
    pub range: Range,
    #[serde(rename = "newText")]
    pub new_text: String,
}

/// LSP envelope for a `WorkspaceEdit`. Servers populate either
/// `changes` (legacy form) or `documentChanges` (newer, preferred
/// form) depending on what the client advertised in `initialize`. We
/// declare `documentChanges: false` but rust-analyzer 1.95+ ignores
/// that capability and always returns `documentChanges` from
/// `textDocument/rename` — see `Self::flatten` for the unified view
/// every consumer should use.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceEdit {
    #[serde(default)]
    pub changes: HashMap<DocumentUri, Vec<TextEdit>>,
    /// Newer form. Each entry carries its own document identifier
    /// plus the edits to apply to it. We accept both `TextDocumentEdit`
    /// objects *and* the file-op variants (`CreateFile`/`RenameFile`/
    /// `DeleteFile`) by deserialising as raw `Value`s; only the
    /// document-edit variant carries `edits`, so [`Self::flatten`]
    /// silently drops the others — they don't apply to a rename.
    #[serde(default, rename = "documentChanges")]
    pub document_changes: Vec<serde_json::Value>,
}

impl WorkspaceEdit {
    /// Collapse `changes` and `documentChanges` into a single
    /// `(uri -> edits)` map, the shape every downstream consumer
    /// (`aa-patch::typed_rename`) actually needs. Entries from
    /// `documentChanges` are merged into the corresponding `changes`
    /// bucket; non-edit file ops (`CreateFile`/`RenameFile`/
    /// `DeleteFile`) are dropped because rename plans don't need
    /// them.
    pub fn flatten(&self) -> HashMap<DocumentUri, Vec<TextEdit>> {
        let mut out: HashMap<DocumentUri, Vec<TextEdit>> = self.changes.clone();
        for raw in &self.document_changes {
            // Only TextDocumentEdit carries `edits`. The file-op
            // variants have a `kind` field set to `"create" |
            // "rename" | "delete"` instead — skip them.
            if raw.get("kind").is_some() {
                continue;
            }
            let Some(uri_str) = raw
                .get("textDocument")
                .and_then(|td| td.get("uri"))
                .and_then(|u| u.as_str())
            else {
                continue;
            };
            let Some(arr) = raw.get("edits").and_then(|e| e.as_array()) else {
                continue;
            };
            let edits: Vec<TextEdit> = arr
                .iter()
                .filter_map(|v| serde_json::from_value::<TextEdit>(v.clone()).ok())
                .collect();
            if edits.is_empty() {
                continue;
            }
            out.entry(DocumentUri(uri_str.to_string()))
                .or_default()
                .extend(edits);
        }
        out
    }
}

// ---- Request/response envelopes ------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonRpcRequest<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: P,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonRpcNotification<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    pub params: P,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JsonRpcResponse {
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
    #[serde(default)]
    pub method: Option<String>,
}

// ---- initialize params ---------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub(crate) struct InitializeParams {
    #[serde(rename = "processId")]
    pub process_id: Option<u32>,
    #[serde(rename = "rootUri")]
    pub root_uri: DocumentUri,
    #[serde(rename = "workspaceFolders")]
    pub workspace_folders: Vec<WorkspaceFolder>,
    pub capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ClientInfo {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceFolder {
    pub uri: DocumentUri,
    pub name: String,
}

/// We advertise only what the rename flow needs: rename with
/// `prepareSupport = false` keeps the protocol round-trips minimal.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ClientCapabilities {
    pub workspace: WorkspaceCaps,
    #[serde(rename = "textDocument")]
    pub text_document: TextDocumentCaps,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceCaps {
    #[serde(rename = "workspaceEdit")]
    pub workspace_edit: WorkspaceEditCaps,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceEditCaps {
    #[serde(rename = "documentChanges")]
    pub document_changes: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TextDocumentCaps {
    pub rename: RenameCaps,
    pub synchronization: SyncCaps,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RenameCaps {
    #[serde(rename = "prepareSupport")]
    pub prepare_support: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SyncCaps {
    #[serde(rename = "didSave")]
    pub did_save: bool,
}

// ---- didOpen / rename params --------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DidOpenTextDocumentParams {
    #[serde(rename = "textDocument")]
    pub text_document: TextDocumentItem,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TextDocumentItem {
    pub uri: DocumentUri,
    #[serde(rename = "languageId")]
    pub language_id: &'static str,
    pub version: i32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RenameParams<'a> {
    #[serde(rename = "textDocument")]
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
    #[serde(rename = "newName")]
    pub new_name: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TextDocumentIdentifier {
    pub uri: DocumentUri,
}

// ---- didChange params ----------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DidChangeTextDocumentParams {
    #[serde(rename = "textDocument")]
    pub text_document: VersionedTextDocumentIdentifier,
    #[serde(rename = "contentChanges")]
    pub content_changes: Vec<TextDocumentContentChangeEvent>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct VersionedTextDocumentIdentifier {
    pub uri: DocumentUri,
    pub version: i32,
}

/// Full-text replace variant — omits the `range` field, which tells RA
/// (and any LSP server) that `text` is the complete new contents of the
/// document. Much simpler than tracking incremental edits, and a good
/// fit for the persistent-session use case: the Core already owns the
/// full file text.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TextDocumentContentChangeEvent {
    pub text: String,
}
