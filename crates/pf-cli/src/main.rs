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
    EvidenceNodeDto, ExplainPatchParams, ExplainPatchResult, Id, IngestFactParams,
    InitializeParams, LlmProposeParams, LlmProposePatchParams, LlmProposePatchResult,
    LlmProposeResult, LlmRefineParams, LlmRefineResult, PatchApplyParams, PatchApplyResult,
    PatchPlanDto, PatchPreviewParams, PatchPreviewResult, PatchRollbackParams, PatchRollbackResult,
    QueryParams, QueryResult, Request, Response, RulesEvaluateParams, RulesEvaluateResult,
    RulesLoadParams, RulesLoadResult, ServerCapabilities, VerdictDto, WorkspaceId,
    WorkspaceIndexParams, WorkspaceIndexResult, WorkspaceOpenParams, WorkspaceOpenResult,
    METHOD_EXPLAIN_PATCH, METHOD_GRAPH_QUERY, METHOD_INITIALIZE, METHOD_LLM_PROPOSE,
    METHOD_LLM_PROPOSE_PATCH, METHOD_LLM_REFINE, METHOD_PATCH_APPLY, METHOD_PATCH_PREVIEW,
    METHOD_PATCH_ROLLBACK, METHOD_RULES_EVALUATE, METHOD_RULES_LOAD, METHOD_WORKSPACE_INDEX,
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
        /// Actually write the patch to disk after validation. Without this
        /// flag the command only prints the preview; the filesystem is
        /// never touched.
        #[arg(long)]
        apply: bool,
        /// Run `cargo check` on a shadow copy of the workspace as part of
        /// validation (validation_profile = "typed"). Requires `cargo` on
        /// PATH. Slower than the default pipeline but upgrades the
        /// verdict from `not_proven` to `accepted`.
        #[arg(long)]
        typecheck: bool,
        /// Additionally run `cargo test --no-fail-fast` against the
        /// shadow copy (validation_profile = "tested"). Implies
        /// `--typecheck`. Strongest behavioral gate; substantially
        /// slower than the default.
        #[arg(long)]
        run_tests: bool,
    },
    /// Roll back a previously applied commit, restoring the workspace to
    /// its pre-commit state. Refuses if the on-disk content no longer
    /// matches what was written at commit time.
    Rollback {
        /// Workspace root.
        root: PathBuf,
        /// Commit id returned by `pf rename --apply` (or any other apply).
        commit_id: String,
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
    /// Run the neuro-symbolic refinement loop: `propose` once, then feed
    /// the rejection reasons back to the model up to `rounds` times until
    /// every surviving candidate resolves cleanly against the graph.
    Refine {
        /// Workspace root to index.
        root: PathBuf,
        /// Entity id to anchor the context selection.
        #[arg(long)]
        anchor: String,
        /// Natural-language intent passed to the orchestrator.
        #[arg(long, default_value = "refine invariants after validator feedback")]
        intent: String,
        /// Context radius (hops around the anchor).
        #[arg(long, default_value = "1")]
        hops: usize,
        /// Maximum refinement rounds (includes the initial pass). The
        /// loop exits earlier if a round produces zero rejections.
        #[arg(long, default_value = "3")]
        rounds: u32,
    },
    /// Ask the LLM orchestrator for *typed patch plan* candidates and,
    /// for every grounded one, immediately run `explain.patch` to
    /// synthesize a proof-carrying verdict. This is the full
    /// neuro-symbolic loop end-to-end: LLM proposes *what to do* in a
    /// bounded op vocabulary, the symbolic side proves each suggestion
    /// is safe.
    ProposePatch {
        /// Workspace root to index.
        root: PathBuf,
        /// Entity id to anchor the context selection.
        #[arg(long)]
        anchor: String,
        /// Natural-language intent passed to the orchestrator.
        #[arg(long, default_value = "propose a typed patch plan for this area")]
        intent: String,
        /// Context radius.
        #[arg(long, default_value = "1")]
        hops: usize,
        /// Validation profile used when explaining each grounded
        /// candidate (`default` | `typed` | `tested`).
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Produce a proof-carrying explanation for a rename plan: which
    /// observed facts are cited, which rules fire on the shadow graph,
    /// which candidates were considered, which validation stages ran, and
    /// what the final verdict is. The filesystem is never touched.
    Explain {
        /// Workspace root to index.
        root: PathBuf,
        /// Source identifier for a `rename_function` plan.
        #[arg(long)]
        from: String,
        /// Target identifier for a `rename_function` plan.
        #[arg(long)]
        to: String,
        /// Emit the full evidence stream. Default: summary + verdict only.
        #[arg(long)]
        verbose: bool,
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
        Cmd::Refine {
            root,
            anchor,
            intent,
            hops,
            rounds,
        } => cmd_refine(root, anchor, intent, hops, rounds),
        Cmd::ProposePatch {
            root,
            anchor,
            intent,
            hops,
            profile,
        } => cmd_propose_patch(root, anchor, intent, hops, profile),
        Cmd::Explain {
            root,
            from,
            to,
            verbose,
        } => cmd_explain(root, from, to, verbose),
        Cmd::Rename {
            root,
            from,
            to,
            apply,
            typecheck,
            run_tests,
        } => cmd_rename(root, from, to, apply, typecheck, run_tests),
        Cmd::Rollback { root, commit_id } => cmd_rollback(root, commit_id),
    }
}

fn cmd_rollback(root: PathBuf, commit_id: String) -> Result<()> {
    let core = Core::new();
    let resp = call(
        &core,
        METHOD_WORKSPACE_OPEN,
        serde_json::to_value(WorkspaceOpenParams {
            root: root.display().to_string(),
        })?,
    )?;
    let ws = serde_json::from_value::<WorkspaceOpenResult>(resp)?.workspace_id;

    let resp = call(
        &core,
        METHOD_PATCH_ROLLBACK,
        serde_json::to_value(PatchRollbackParams {
            workspace_id: ws,
            commit_id: commit_id.clone(),
        })?,
    )?;
    let r: PatchRollbackResult = serde_json::from_value(resp)?;
    if r.rolled_back {
        println!(
            "rolled back: commit {} ({}), {} file(s) restored",
            r.commit_id, r.label, r.files_restored
        );
        Ok(())
    } else {
        eprintln!(
            "rollback failed for {}: {}",
            commit_id,
            r.reason.unwrap_or_else(|| "unknown".into())
        );
        std::process::exit(2);
    }
}

fn cmd_rename(
    root: PathBuf,
    from: String,
    to: String,
    apply: bool,
    typecheck: bool,
    run_tests: bool,
) -> Result<()> {
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

    // Always show the preview first so the user sees what will happen.
    let resp = call(
        &core,
        METHOD_PATCH_PREVIEW,
        serde_json::to_value(PatchPreviewParams {
            workspace_id: ws.clone(),
            plan: plan.clone(),
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

    if !apply {
        return Ok(());
    }

    let resp = call(
        &core,
        METHOD_PATCH_APPLY,
        serde_json::to_value(PatchApplyParams {
            workspace_id: ws,
            plan,
            validation_profile: if run_tests {
                Some("tested".into())
            } else if typecheck {
                Some("typed".into())
            } else {
                None
            },
        })?,
    )?;
    let result: PatchApplyResult = serde_json::from_value(resp)?;
    println!();
    if result.applied {
        println!(
            "applied: commit {} ({} file(s), {} bytes)",
            result.commit_id.as_deref().unwrap_or("-"),
            result.files_written,
            result.bytes_written
        );
    } else {
        println!(
            "rejected: {}",
            result.rejection_reason.as_deref().unwrap_or("unknown")
        );
    }
    if !result.validation.ok {
        println!("validation failures:");
        for st in &result.validation.stages {
            if st.ok {
                continue;
            }
            println!("  [{}]:", st.stage);
            for d in &st.diagnostics {
                let where_ = st.diagnostics.iter().find_map(|x| x.file.clone());
                println!(
                    "    {}: {} ({})",
                    d.severity,
                    d.message,
                    where_.as_deref().unwrap_or("-")
                );
            }
        }
    }
    if !result.applied {
        std::process::exit(2);
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

fn cmd_refine(
    root: PathBuf,
    anchor: String,
    intent: String,
    hops: usize,
    rounds: u32,
) -> Result<()> {
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
        METHOD_LLM_REFINE,
        serde_json::to_value(LlmRefineParams {
            workspace_id: ws,
            intent,
            anchor_id: anchor,
            hops,
            max_facts: 256,
            max_rounds: rounds,
            prior_outcomes: Vec::new(),
            prior_diagnostics: Vec::new(),
        })?,
    )?;
    let r: LlmRefineResult = serde_json::from_value(resp)?;
    println!(
        "refine: {} round(s), converged={}, accepted={}, rejected={} \
         (tokens_in={}, tokens_out={})",
        r.rounds,
        r.converged,
        r.final_accepted,
        r.final_rejected,
        r.tokens_in_total,
        r.tokens_out_total
    );
    for rs in &r.rounds_summary {
        println!(
            "  round {}: accepted={} rejected={} cache_hit={} tokens={}+{}",
            rs.round, rs.accepted, rs.rejected, rs.cache_hit, rs.tokens_in, rs.tokens_out
        );
    }
    for o in &r.outcomes {
        let status = if o.accepted { "  ACCEPT" } else { "  REJECT" };
        let round_tag = o
            .round
            .map(|r| format!(" r{}", r))
            .unwrap_or_else(|| " r?".into());
        println!(
            "{}{} {}({}) — {}{}",
            status,
            round_tag,
            o.predicate,
            o.args.join(", "),
            o.justification,
            o.rejection_reason
                .as_deref()
                .map(|r| format!("  [why: {r}]"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn cmd_propose_patch(
    root: PathBuf,
    anchor: String,
    intent: String,
    hops: usize,
    profile: String,
) -> Result<()> {
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
        METHOD_LLM_PROPOSE_PATCH,
        serde_json::to_value(LlmProposePatchParams {
            workspace_id: ws.clone(),
            intent,
            anchor_id: anchor,
            hops,
            max_facts: 256,
        })?,
    )?;
    let r: LlmProposePatchResult = serde_json::from_value(resp)?;
    println!(
        "propose_patch: accepted {} / rejected {} (cache_hit={}, tokens_in={}, tokens_out={})",
        r.accepted, r.rejected, r.cache_hit, r.tokens_in, r.tokens_out
    );
    // Normalize the profile to the wire vocabulary; "default" becomes
    // None so the default pipeline runs.
    let validation_profile = match profile.as_str() {
        "default" | "" => None,
        other => Some(other.to_string()),
    };
    for (idx, cand) in r.candidates.iter().enumerate() {
        let head = if cand.accepted { "ACCEPT" } else { "REJECT" };
        println!(
            "  [{idx}] {head} {} — {}",
            cand.plan.label, cand.justification
        );
        if let Some(reason) = &cand.rejection_reason {
            println!("         why: {reason}");
        }
        if !cand.accepted {
            continue;
        }
        // Run `explain.patch` on every accepted plan so the user sees the
        // full proof-carrying verdict in the same output as the proposal.
        let resp = call(
            &core,
            METHOD_EXPLAIN_PATCH,
            serde_json::to_value(ExplainPatchParams {
                workspace_id: ws.clone(),
                plan: cand.plan.clone(),
                candidate_outcomes: Vec::new(),
                validation_profile: validation_profile.clone(),
            })?,
        )?;
        let ex: ExplainPatchResult = serde_json::from_value(resp)?;
        match ex.verdict {
            VerdictDto::Accepted { notes, .. } => {
                println!("         verdict: accepted");
                for n in &notes {
                    println!("           note: {n}");
                }
            }
            VerdictDto::Rejected {
                reason,
                failing_stages,
            } => {
                println!("         verdict: rejected ({reason})");
                for s in &failing_stages {
                    println!("           failing stage: {s}");
                }
            }
            VerdictDto::NotProven { reason } => {
                println!("         verdict: not proven ({reason})");
            }
        }
    }
    Ok(())
}

fn cmd_explain(root: PathBuf, from: String, to: String, verbose: bool) -> Result<()> {
    let core = Core::new();
    let resp = call(
        &core,
        METHOD_WORKSPACE_OPEN,
        serde_json::to_value(WorkspaceOpenParams {
            root: root.display().to_string(),
        })?,
    )?;
    let ws = serde_json::from_value::<WorkspaceOpenResult>(resp)?.workspace_id;

    // Index the workspace so the graph has entity facts. The explainer
    // reads from the graph only; it never writes to disk.
    let _ = call(
        &core,
        METHOD_WORKSPACE_INDEX,
        serde_json::to_value(WorkspaceIndexParams {
            workspace_id: ws.clone(),
        })?,
    )?;

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
        METHOD_EXPLAIN_PATCH,
        serde_json::to_value(ExplainPatchParams {
            workspace_id: ws,
            plan,
            candidate_outcomes: Vec::new(),
            validation_profile: None,
        })?,
    )?;
    let r: ExplainPatchResult = serde_json::from_value(resp)?;

    println!("{}", r.summary);
    match &r.verdict {
        VerdictDto::Accepted { commit_id, notes } => {
            println!("verdict: accepted");
            if let Some(id) = commit_id {
                println!("  commit: {id}");
            }
            for n in notes {
                println!("  note: {n}");
            }
        }
        VerdictDto::Rejected {
            reason,
            failing_stages,
        } => {
            println!("verdict: rejected ({reason})");
            for s in failing_stages {
                println!("  failing stage: {s}");
            }
        }
        VerdictDto::NotProven { reason } => {
            println!("verdict: not proven ({reason})");
        }
    }
    println!(
        "stats: {} anchor(s), {} observed, {} inferred, {} rule activation(s), \
         {} candidate(s), {} stage(s)",
        r.stats.anchors,
        r.stats.observed_cited,
        r.stats.inferred_cited,
        r.stats.rule_activations,
        r.stats.candidates_considered,
        r.stats.stages_run
    );

    if verbose {
        println!("evidence:");
        for node in &r.evidence {
            render_evidence(node);
        }
    }
    Ok(())
}

fn render_evidence(n: &EvidenceNodeDto) {
    match n {
        EvidenceNodeDto::Observed {
            predicate,
            args,
            role,
        } => println!("  observed[{role}] {}({})", predicate, args.join(", ")),
        EvidenceNodeDto::Inferred { predicate, args } => {
            println!("  inferred       {}({})", predicate, args.join(", "))
        }
        EvidenceNodeDto::RuleActivation {
            rule_index,
            head,
            premises,
        } => {
            println!(
                "  rule[{rule_index}]      {}({}) :-",
                head.predicate,
                head.args.join(", ")
            );
            for p in premises {
                println!(
                    "                    {}({}).",
                    p.predicate,
                    p.args.join(", ")
                );
            }
        }
        EvidenceNodeDto::Candidate {
            predicate,
            args,
            justification,
            accepted,
            rejection_reason,
            round,
        } => {
            let status = if *accepted { "ACCEPT" } else { "REJECT" };
            let round_tag = round
                .map(|r| format!(" r{}", r))
                .unwrap_or_else(|| "".into());
            let why = rejection_reason
                .as_deref()
                .map(|r| format!("  [why: {r}]"))
                .unwrap_or_default();
            println!(
                "  cand[{status}{round_tag}] {}({}) — {}{}",
                predicate,
                args.join(", "),
                justification,
                why
            );
        }
        EvidenceNodeDto::Stage {
            name,
            ok,
            diagnostics,
        } => {
            let status = if *ok { "PASS" } else { "FAIL" };
            println!("  stage[{status}]    {name}");
            for d in diagnostics {
                match &d.file {
                    Some(f) => println!("    {}: {} ({f})", d.severity, d.message),
                    None => println!("    {}: {}", d.severity, d.message),
                }
            }
        }
    }
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
