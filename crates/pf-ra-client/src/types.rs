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

/// The shape `textDocument/rename` returns. rust-analyzer populates `changes`;
/// `documentChanges` is the newer form but is not required for rename.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceEdit {
    #[serde(default)]
    pub changes: HashMap<DocumentUri, Vec<TextEdit>>,
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
