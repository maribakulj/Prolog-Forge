//! Prolog Forge reference CLI.
//!
//! In Phase 0 the CLI embeds the Core directly and speaks to it through the
//! same `dispatch` entry-point used by the daemon. This exercises the
//! protocol types end-to-end without spawning a subprocess, and gives CI a
//! useful check tool without any runtime dependency.
//!
//! A future `--daemon` flag will switch to spawning `pf-daemon` and talking
//! JSON-RPC over its stdio, so that the CLI can be used as a thin remote
//! client too.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pf_core::{dispatch, Core};
use pf_protocol::{
    Id, IngestFactParams, InitializeParams, LlmProposeParams, LlmProposeResult, PatchPlanDto,
    PatchPreviewParams, PatchPreviewResult, QueryParams, QueryResult, Request, Response,
    RulesEvaluateParams, RulesEvaluateResult, RulesLoadParams, RulesLoadResult, ServerCapabilities,
    WorkspaceId, WorkspaceIndexParams, WorkspaceIndexResult, WorkspaceOpenParams,
    WorkspaceOpenResult, METHOD_GRAPH_QUERY, METHOD_INITIALIZE, METHOD_LLM_PROPOSE,
    METHOD_PATCH_PREVIEW, METHOD_RULES_EVALUATE, METHOD_RULES_LOAD, METHOD_WORKSPACE_INDEX,
    METHOD_WORKSPACE_OPEN,
};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "pf", version, about = "Prolog Forge reference CLI")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print server capabilities (protocol version, available methods).
    Info,
    /// Parse a Datalog source file and report syntax errors, if any.
    Check { file: PathBuf },
    /// Load a Datalog source file, run the rule engine to fixpoint, and
    /// print evaluation stats.
    Run { file: PathBuf },
    /// Load, evaluate, then execute one query pattern against the graph.
    Query {
        file: PathBuf,
        /// A single Datalog atom, e.g. `ancestor(X, Y)`.
        pattern: String,
    },
    /// Index a Rust project into the knowledge graph; optionally load a rule
    /// pack, evaluate it, and run a query against the resulting graph.
    Index {
        /// Workspace root to index.
        root: PathBuf,
        /// Optional `.pfr` rule pack to load before evaluation.
        #[arg(long)]
        rules: Option<PathBuf>,
        /// Optional query pattern to run after evaluation.
        #[arg(long)]
        query: Option<String>,
    },
    /// Preview the diff produced by renaming every occurrence of `from` to
    /// `to` across the workspace's Rust files. Does not write to disk.
    Rename {
        /// Workspace root.
        root: PathBuf,
        /// Current identifier name.
        #[arg(long)]
        from: String,
        /// New identifier name.
        #[arg(long)]
        to: String,
    },
    /// Index a Rust project then ask the bounded LLM orchestrator to
    /// propose candidate facts anchored at the given entity id.
    Propose {
        /// Workspace root to index.
        root: PathBuf,
        /// Entity id to anchor the context selection (e.g. a function id).
        #[arg(long)]
        anchor: String,
        /// Natural-language intent passed to the orchestrator.
        #[arg(long, default_value = "propose invariants that a human might validate")]
        intent: String,
        /// Context radius (hops around the anchor).
        #[arg(long, default_value = "1")]
        hops: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Info => cmd_info(),
        Cmd::Check { file } => cmd_check(file),
        Cmd::Run { file } => cmd_run(file),
        Cmd::Query { file, pattern } => cmd_query(file, pattern),
        Cmd::Index { root, rules, query } => cmd_index(root, rules, query),
        Cmd::Propose {
            root,
            anchor,
            intent,
            hops,
        } => cmd_propose(root, anchor, intent, hops),
        Cmd::Rename { root, from, to } => cmd_rename(root, from, to),
    }
}

fn cmd_rename(root: PathBuf, from: String, to: String) -> Result<()> {
    let core = Core::new();
    let resp = call(
        &core,
        METHOD_WORKSPACE_OPEN,
        serde_json::to_value(WorkspaceOpenParams {
            root: root.display().to_string(),
        })?,
    )?;
    let ws = serde_json::from_value::<WorkspaceOpenResult>(resp)?.workspace_id;

    let op = serde_json::json!({
        "op": "rename_function",
        "old_name": from,
        "new_name": to,
        "files": []
    });
    let plan = PatchPlanDto {
        ops: vec![op],
        label: format!("rename {from} -> {to}"),
    };
    let resp = call(
        &core,
        METHOD_PATCH_PREVIEW,
        serde_json::to_value(PatchPreviewParams {
            workspace_id: ws,
            plan,
        })?,
    )?;
    let preview: PatchPreviewResult = serde_json::from_value(resp)?;
    println!(
        "preview: {} replacement(s) across {} file(s)",
        preview.total_replacements,
        preview.files.len()
    );
    for e in &preview.errors {
        eprintln!("  error in {}: {}", e.file, e.message);
    }
    for f in &preview.files {
        println!(
            "\n# {} ({} bytes -> {} bytes, {} replacements)",
            f.path, f.before_len, f.after_len, f.replacements
        );
        println!("{}", f.diff);
    }
    Ok(())
}

fn cmd_propose(root: PathBuf, anchor: String, intent: String, hops: usize) -> Result<()> {
    let core = Core::new();
    let resp = call(
        &core,
        METHOD_WORKSPACE_OPEN,
        serde_json::to_value(WorkspaceOpenParams {
            root: root.display().to_string(),
        })?,
    )?;
    let ws = serde_json::from_value::<WorkspaceOpenResult>(resp)?.workspace_id;

    let _ = call(
        &core,
        METHOD_WORKSPACE_INDEX,
        serde_json::to_value(WorkspaceIndexParams {
            workspace_id: ws.clone(),
        })?,
    )?;

    let resp = call(
        &core,
        METHOD_LLM_PROPOSE,
        serde_json::to_value(LlmProposeParams {
            workspace_id: ws,
            intent,
            anchor_id: anchor,
            hops,
            max_facts: 256,
        })?,
    )?;
    let r: LlmProposeResult = serde_json::from_value(resp)?;
    println!(
        "propose: accepted {} / rejected {} (cache_hit={}, tokens_in={}, tokens_out={})",
        r.accepted, r.rejected, r.cache_hit, r.tokens_in, r.tokens_out
    );
    for o in r.outcomes {
        let status = if o.accepted { "  ACCEPT" } else { "  REJECT" };
        println!(
            "{} {}({}) — {}{}",
            status,
            o.predicate,
            o.args.join(", "),
            o.justification,
            o.rejection_reason
                .map(|r| format!("  [why: {r}]"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn cmd_index(root: PathBuf, rules: Option<PathBuf>, query: Option<String>) -> Result<()> {
    let core = Core::new();
    let resp = call(
        &core,
        METHOD_WORKSPACE_OPEN,
        serde_json::to_value(WorkspaceOpenParams {
            root: root.display().to_string(),
        })?,
    )?;
    let open: WorkspaceOpenResult = serde_json::from_value(resp)?;
    let ws = open.workspace_id;

    let resp = call(
        &core,
        METHOD_WORKSPACE_INDEX,
        serde_json::to_value(WorkspaceIndexParams {
            workspace_id: ws.clone(),
        })?,
    )?;
    let report: WorkspaceIndexResult = serde_json::from_value(resp)?;
    println!(
        "indexed: {} file(s), {} entity(ies), {} relation(s), {} fact(s); failed: {}",
        report.files_indexed,
        report.entities,
        report.relations,
        report.facts_inserted,
        report.files_failed
    );
    for e in &report.errors {
        eprintln!("  error in {}: {}", e.file, e.message);
    }

    if let Some(rules_path) = rules {
        let src = fs::read_to_string(&rules_path)
            .with_context(|| format!("reading {}", rules_path.display()))?;
        let _ = call(
            &core,
            METHOD_RULES_LOAD,
            serde_json::to_value(RulesLoadParams {
                workspace_id: ws.clone(),
                source: src,
            })?,
        )?;
        let stats = eval(&core, &ws)?;
        println!(
            "rule eval: derived {} fact(s) in {} iteration(s)",
            stats.derived, stats.iterations
        );
    }

    if let Some(pattern) = query {
        let resp = call(
            &core,
            METHOD_GRAPH_QUERY,
            serde_json::to_value(QueryParams {
                workspace_id: ws,
                pattern,
            })?,
        )?;
        let qr: QueryResult = serde_json::from_value(resp)?;
        println!("query: {} result(s)", qr.count);
        for b in qr.bindings {
            println!("  {}", b);
        }
    }
    Ok(())
}

fn cmd_info() -> Result<()> {
    let core = Core::new();
    let resp = call(
        &core,
        METHOD_INITIALIZE,
        serde_json::to_value(InitializeParams {
            client: pf_protocol::ClientCapabilities {
                name: "pf-cli".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        })?,
    )?;
    let caps: ServerCapabilities = serde_json::from_value(resp)?;
    println!("name: {}", caps.name);
    println!("version: {}", caps.version);
    println!("protocol: {}", caps.protocol_version);
    println!("methods:");
    for m in caps.methods {
        println!("  - {m}");
    }
    Ok(())
}

fn cmd_check(file: PathBuf) -> Result<()> {
    let src = fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    match pf_rules_stub::parse(&src) {
        Ok((rules, facts)) => {
            println!("ok: {} rule(s), {} fact(s)", rules, facts);
            Ok(())
        }
        Err(e) => {
            eprintln!("parse error: {e}");
            std::process::exit(2);
        }
    }
}

fn cmd_run(file: PathBuf) -> Result<()> {
    let (core, ws) = open(&file)?;
    let stats = eval(&core, &ws)?;
    println!("derived: {}", stats.derived);
    println!("iterations: {}", stats.iterations);
    Ok(())
}

fn cmd_query(file: PathBuf, pattern: String) -> Result<()> {
    let (core, ws) = open(&file)?;
    let _ = eval(&core, &ws)?;
    let resp = call(
        &core,
        METHOD_GRAPH_QUERY,
        serde_json::to_value(QueryParams {
            workspace_id: ws,
            pattern,
        })?,
    )?;
    let qr: QueryResult = serde_json::from_value(resp)?;
    println!("{} result(s)", qr.count);
    for b in qr.bindings {
        println!("  {}", b);
    }
    Ok(())
}

fn open(file: &PathBuf) -> Result<(Core, WorkspaceId)> {
    let src = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let core = Core::new();
    let resp = call(
        &core,
        METHOD_WORKSPACE_OPEN,
        serde_json::to_value(WorkspaceOpenParams {
            root: file
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .display()
                .to_string(),
        })?,
    )?;
    let open: WorkspaceOpenResult = serde_json::from_value(resp)?;
    let ws = open.workspace_id;
    let _resp = call(
        &core,
        METHOD_RULES_LOAD,
        serde_json::to_value(RulesLoadParams {
            workspace_id: ws.clone(),
            source: src,
        })?,
    )?;
    Ok((core, ws))
}

fn eval(core: &Core, ws: &WorkspaceId) -> Result<RulesEvaluateResult> {
    let resp = call(
        core,
        METHOD_RULES_EVALUATE,
        serde_json::to_value(RulesEvaluateParams {
            workspace_id: ws.clone(),
        })?,
    )?;
    Ok(serde_json::from_value(resp)?)
}

fn call(core: &Core, method: &str, params: Value) -> Result<Value> {
    let req = Request {
        jsonrpc: "2.0".into(),
        method: method.into(),
        params: Some(params),
        id: Some(Id::Num(1)),
    };
    let resp: Response = dispatch(core, req).expect("request has id; response must exist");
    if let Some(err) = resp.error {
        anyhow::bail!("{} failed: {}", method, err.message);
    }
    Ok(resp.result.unwrap_or(Value::Null))
}

// Silence an unused-import lint on IngestFactParams / json, which exist to
// document the available API surface from within the reference CLI crate.
#[allow(dead_code)]
fn _unused_api_surface() {
    let _: Option<IngestFactParams> = None;
    let _ = json!(null);
    let _ = RulesLoadResult {
        rules_added: 0,
        facts_added: 0,
    };
}

// Tiny wrapper so we don't pull pf-rules directly into the CLI dep graph
// beyond what we need for the `check` subcommand.
mod pf_rules_stub {
    pub fn parse(src: &str) -> Result<(usize, usize), String> {
        match pf_rules::parse(src) {
            Ok(p) => Ok((p.rules.len(), p.facts.len())),
            Err(e) => Err(e.to_string()),
        }
    }
}
