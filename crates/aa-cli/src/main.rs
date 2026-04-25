//! AYE-AYE reference CLI.
//!
//! In Phase 0 the CLI embeds the Core directly and speaks to it through the
//! same `dispatch` entry-point used by the daemon. This exercises the
//! protocol types end-to-end without spawning a subprocess, and gives CI a
//! useful check tool without any runtime dependency.
//!
//! A future `--daemon` flag will switch to spawning `aa-daemon` and talking
//! JSON-RPC over its stdio, so that the CLI can be used as a thin remote
//! client too.

use std::fs;
use std::path::PathBuf;

use aa_core::{dispatch, Core};
use aa_protocol::{
    EvidenceNodeDto, ExplainPatchParams, ExplainPatchResult, Id, IngestFactParams,
    InitializeParams, LlmProposeParams, LlmProposePatchParams, LlmProposePatchResult,
    LlmProposeResult, LlmRefineParams, LlmRefineResult, MemoryGetParams, MemoryGetResult,
    MemoryHistoryParams, MemoryHistoryResult, MemoryStatsParams, MemoryStatsResult,
    PatchApplyParams, PatchApplyResult, PatchPlanDto, PatchPreviewParams, PatchPreviewResult,
    PatchRollbackParams, PatchRollbackResult, QueryParams, QueryResult, Request, Response,
    RulesEvaluateParams, RulesEvaluateResult, RulesLoadParams, RulesLoadResult, ServerCapabilities,
    VerdictDto, WorkspaceId, WorkspaceIndexParams, WorkspaceIndexResult, WorkspaceOpenParams,
    WorkspaceOpenResult, METHOD_EXPLAIN_PATCH, METHOD_GRAPH_QUERY, METHOD_INITIALIZE,
    METHOD_LLM_PROPOSE, METHOD_LLM_PROPOSE_PATCH, METHOD_LLM_REFINE, METHOD_MEMORY_GET,
    METHOD_MEMORY_HISTORY, METHOD_MEMORY_STATS, METHOD_PATCH_APPLY, METHOD_PATCH_PREVIEW,
    METHOD_PATCH_ROLLBACK, METHOD_RULES_EVALUATE, METHOD_RULES_LOAD, METHOD_WORKSPACE_INDEX,
    METHOD_WORKSPACE_OPEN,
};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "aa", version, about = "AYE-AYE reference CLI")]
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
        /// Use `rust-analyzer` for scope-resolved rename (Step 2 of
        /// the type-aware rename ladder). Distinguishes a local
        /// variable `add` from a function `add`; only renames actual
        /// references to the symbol. Requires `rust-analyzer` on
        /// PATH. When RA is absent, the op is skipped with a
        /// diagnostic (use the default macro-aware rename instead).
        #[arg(long)]
        scope_resolved: bool,
    },
    /// Roll back a previously applied commit, restoring the workspace to
    /// its pre-commit state. Refuses if the on-disk content no longer
    /// matches what was written at commit time.
    Rollback {
        /// Workspace root.
        root: PathBuf,
        /// Commit id returned by `aa rename --apply` (or any other apply).
        commit_id: String,
    },
    /// List committed patches newest-first — the queryable view of
    /// the runtime's journal. Phase 1.14's "what have I done on this
    /// repo" surface.
    History {
        /// Workspace root.
        root: PathBuf,
        /// Optional filter: only entries whose label starts with this.
        #[arg(long)]
        label_prefix: Option<String>,
        /// Optional filter: only entries that include this op tag
        /// (`rename_function`, `rename_function_typed`,
        /// `add_derive_to_struct`).
        #[arg(long)]
        op_tag: Option<String>,
        /// Optional filter: only entries with this validation profile
        /// (`default`, `typed`, `tested`).
        #[arg(long)]
        profile: Option<String>,
        /// Cap the output at N entries.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Show one committed patch's full journal entry (metadata +
    /// before/after bytes of every file).
    Show {
        /// Workspace root.
        root: PathBuf,
        commit_id: String,
    },
    /// Aggregate stats over the whole journal: count by op kind, by
    /// validation profile, top-N most-edited files.
    Stats {
        /// Workspace root.
        root: PathBuf,
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
        /// Phase 1.15: feed the orchestrator the N most recent
        /// commits from this repo's journal so proposals can be
        /// biased toward shapes that have historically succeeded
        /// here. `0` (default) disables memory and reuses the v1
        /// prompt path.
        #[arg(long, default_value = "0")]
        include_memory: usize,
    },
    /// Add one or more derives to a struct / enum / union. Merges into
    /// any existing `#[derive(...)]` on the target, skipping duplicates;
    /// inserts a fresh `#[derive(...)]` attribute above the item
    /// otherwise. Preview is pure; pass `--apply` to write.
    AddDerive {
        /// Workspace root.
        root: PathBuf,
        /// Name of the target struct/enum/union (no path — see the
        /// `add_derive_to_struct` op in the protocol for the full
        /// shape).
        #[arg(long = "type")]
        type_name: String,
        /// Comma-separated list of derive paths, e.g.
        /// `Debug,Clone,serde::Serialize`.
        #[arg(long)]
        derives: String,
        /// Actually write the patch to disk after validation.
        #[arg(long)]
        apply: bool,
    },
    /// Dual of `add-derive`: drop one or more derives from a struct /
    /// enum / union's `#[derive(...)]` attribute. If every listed
    /// derive is absent the op is a no-op (idempotent); if the
    /// filter empties the derive list, the whole `#[derive(...)]`
    /// attribute line is stripped.
    RemoveDerive {
        /// Workspace root.
        root: PathBuf,
        #[arg(long = "type")]
        type_name: String,
        /// Comma-separated list of derive paths to drop.
        #[arg(long)]
        derives: String,
        /// Actually write the patch to disk after validation.
        #[arg(long)]
        apply: bool,
    },
    /// Inline a free-standing function: substitute every bare call site
    /// with the function's body (wrapped in a block that binds each
    /// parameter to its argument) and then remove the function
    /// definition. Refuses recursion, `return` in the body,
    /// `async/const/unsafe`, generics, `self` receivers, macro-body
    /// call sites, and any non-bare reference in scope (qualified
    /// paths, `use` re-exports) that would dangle after removal.
    InlineFunction {
        /// Workspace root.
        root: PathBuf,
        /// Name of the function to inline.
        #[arg(long = "function")]
        function: String,
        /// Actually write the patch to disk after validation.
        #[arg(long)]
        apply: bool,
    },
    /// Phase 1.22 — dual of `inline-function`. Lift a contiguous run
    /// of statements out of a free-standing fn body into a new helper,
    /// replacing the original site with a call. The selection is given
    /// as a 1-indexed inclusive line range; parameters of the new
    /// helper are listed explicitly as `name:type` pairs.
    ExtractFunction {
        /// Workspace root.
        root: PathBuf,
        /// Workspace-relative path of the file to edit.
        #[arg(long = "file")]
        source_file: String,
        /// 1-indexed inclusive start line of the selection.
        #[arg(long)]
        start_line: u32,
        /// 1-indexed inclusive end line of the selection.
        #[arg(long)]
        end_line: u32,
        /// Name of the new helper.
        #[arg(long = "name")]
        new_name: String,
        /// Parameter list for the new helper. Format:
        /// `name1:type1,name2:type2`. Each name must appear in the
        /// selection; each type must parse as a Rust type. Empty
        /// list (no `--params`) is fine for parameter-less helpers.
        #[arg(long, default_value = "")]
        params: String,
        /// Actually write the patch to disk after validation.
        #[arg(long)]
        apply: bool,
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
            include_memory,
        } => cmd_propose_patch(root, anchor, intent, hops, profile, include_memory),
        Cmd::AddDerive {
            root,
            type_name,
            derives,
            apply,
        } => cmd_add_derive(root, type_name, derives, apply),
        Cmd::RemoveDerive {
            root,
            type_name,
            derives,
            apply,
        } => cmd_remove_derive(root, type_name, derives, apply),
        Cmd::InlineFunction {
            root,
            function,
            apply,
        } => cmd_inline_function(root, function, apply),
        Cmd::ExtractFunction {
            root,
            source_file,
            start_line,
            end_line,
            new_name,
            params,
            apply,
        } => cmd_extract_function(
            root,
            source_file,
            start_line,
            end_line,
            new_name,
            params,
            apply,
        ),
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
            scope_resolved,
        } => cmd_rename(root, from, to, apply, typecheck, run_tests, scope_resolved),
        Cmd::Rollback { root, commit_id } => cmd_rollback(root, commit_id),
        Cmd::History {
            root,
            label_prefix,
            op_tag,
            profile,
            limit,
        } => cmd_history(root, label_prefix, op_tag, profile, limit),
        Cmd::Show { root, commit_id } => cmd_show(root, commit_id),
        Cmd::Stats { root } => cmd_stats(root),
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

/// Walk `.rs` files under `root` and return `(relative_path, 0-indexed
/// line, 0-indexed character)` for the first `fn <name>` declaration we
/// find. Used by `aa rename --scope-resolved` to hand rust-analyzer a
/// position to resolve the symbol from. Returns `None` if no matching
/// declaration is found — the caller turns that into a clear error so
/// the user knows to check the name or fall back to the syntactic
/// rename.
fn find_fn_decl(root: &std::path::Path, name: &str) -> Option<(String, u32, u32)> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            let file_name = entry.file_name();
            let name_s = file_name.to_string_lossy();
            if p.is_dir() {
                if name_s == "target" || name_s == ".aye-aye" || name_s == ".git" {
                    continue;
                }
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Ok(src) = fs::read_to_string(&p) else {
                continue;
            };
            if let Some((line, character)) = find_fn_in_source(&src, name) {
                let rel = p.strip_prefix(root).ok()?.to_string_lossy().into_owned();
                return Some((rel, line, character));
            }
        }
    }
    None
}

/// Parse `src` with syn, visit every `ItemFn` / `ImplItemFn` / `TraitItemFn`,
/// and return the position of the first identifier whose string equals
/// `name`. 0-indexed line + character to match LSP's position shape.
fn find_fn_in_source(src: &str, name: &str) -> Option<(u32, u32)> {
    use syn::visit::Visit;
    struct Finder<'a> {
        name: &'a str,
        found: Option<(u32, u32)>,
    }
    impl<'a, 'ast> Visit<'ast> for Finder<'a> {
        fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
            self.check(&i.sig.ident);
            syn::visit::visit_item_fn(self, i);
        }
        fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
            self.check(&i.sig.ident);
            syn::visit::visit_impl_item_fn(self, i);
        }
        fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
            self.check(&i.sig.ident);
            syn::visit::visit_trait_item_fn(self, i);
        }
    }
    impl<'a> Finder<'a> {
        fn check(&mut self, ident: &proc_macro2::Ident) {
            if self.found.is_some() || ident != self.name {
                return;
            }
            let span = ident.span();
            let start = span.start();
            // `proc_macro2::LineColumn::line` is 1-indexed, `column`
            // is 0-indexed byte offset on the line. LSP wants both
            // 0-indexed; for ASCII Rust identifiers byte == UTF-16
            // code-unit count, so we pass column through unchanged.
            if start.line >= 1 {
                self.found = Some(((start.line - 1) as u32, start.column as u32));
            }
        }
    }
    let file = syn::parse_file(src).ok()?;
    let mut f = Finder { name, found: None };
    f.visit_file(&file);
    f.found
}

fn cmd_rename(
    root: PathBuf,
    from: String,
    to: String,
    apply: bool,
    typecheck: bool,
    run_tests: bool,
    scope_resolved: bool,
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

    let op = if scope_resolved {
        // Locate any `fn <from>` declaration to hand RA a position.
        // Syn gives us line+column (0-indexed line, 0-indexed UTF-16
        // character) on the identifier span; LSP wants the same shape.
        let (decl_file, line, character) = find_fn_decl(&root, &from).ok_or_else(|| {
            anyhow::anyhow!(
                "scope-resolved rename: no `fn {from}` declaration found under {}",
                root.display()
            )
        })?;
        serde_json::json!({
            "op": "rename_function_typed",
            "decl_file": decl_file,
            "decl_line": line,
            "decl_character": character,
            "new_name": to,
            "old_name": from,
        })
    } else {
        serde_json::json!({
            "op": "rename_function",
            "old_name": from,
            "new_name": to,
            "files": []
        })
    };
    let plan = PatchPlanDto {
        ops: vec![op],
        label: if scope_resolved {
            format!("rename {from} -> {to} (scope-resolved)")
        } else {
            format!("rename {from} -> {to}")
        },
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
    include_memory: usize,
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
            include_memory: if include_memory > 0 {
                Some(include_memory)
            } else {
                None
            },
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

fn cmd_add_derive(
    root: PathBuf,
    type_name: String,
    derives_csv: String,
    apply: bool,
) -> Result<()> {
    let derives: Vec<String> = derives_csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if derives.is_empty() {
        anyhow::bail!("--derives must list at least one trait name");
    }

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
        "op": "add_derive_to_struct",
        "type_name": type_name,
        "derives": derives,
        "files": [],
    });
    let plan = PatchPlanDto {
        ops: vec![op],
        label: format!("add derive({}) to {type_name}", derives.join(", ")),
    };

    // Preview first, same shape as `aa rename`.
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
        "preview: {} new derive(s) across {} file(s)",
        preview.total_replacements,
        preview.files.len()
    );
    for e in &preview.errors {
        eprintln!("  error in {}: {}", e.file, e.message);
    }
    for f in &preview.files {
        println!(
            "\n# {} ({} bytes -> {} bytes, {} new derive(s))",
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
            validation_profile: None,
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
        std::process::exit(2);
    }
    Ok(())
}

fn cmd_remove_derive(
    root: PathBuf,
    type_name: String,
    derives_csv: String,
    apply: bool,
) -> Result<()> {
    let derives: Vec<String> = derives_csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if derives.is_empty() {
        anyhow::bail!("--derives must list at least one trait name");
    }

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
        "op": "remove_derive_from_struct",
        "type_name": type_name,
        "derives": derives,
        "files": [],
    });
    let plan = PatchPlanDto {
        ops: vec![op],
        label: format!("remove derive({}) from {type_name}", derives.join(", ")),
    };

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
        "preview: {} derive(s) removed across {} file(s)",
        preview.total_replacements,
        preview.files.len()
    );
    for e in &preview.errors {
        eprintln!("  error in {}: {}", e.file, e.message);
    }
    for f in &preview.files {
        println!(
            "\n# {} ({} bytes -> {} bytes, {} removal(s))",
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
            validation_profile: None,
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
        std::process::exit(2);
    }
    Ok(())
}

fn cmd_inline_function(root: PathBuf, function: String, apply: bool) -> Result<()> {
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
        "op": "inline_function",
        "function": function,
        "files": [],
    });
    let plan = PatchPlanDto {
        ops: vec![op],
        label: format!("inline function {function}"),
    };

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
        "preview: {} byte-level edit(s) across {} file(s)",
        preview.total_replacements,
        preview.files.len()
    );
    for e in &preview.errors {
        eprintln!("  error in {}: {}", e.file, e.message);
    }
    for f in &preview.files {
        println!(
            "\n# {} ({} bytes -> {} bytes, {} edit(s))",
            f.path, f.before_len, f.after_len, f.replacements
        );
        println!("{}", f.diff);
    }

    if !apply {
        return Ok(());
    }
    if !preview.errors.is_empty() {
        println!("refusing to apply: preview reported errors");
        std::process::exit(2);
    }

    let resp = call(
        &core,
        METHOD_PATCH_APPLY,
        serde_json::to_value(PatchApplyParams {
            workspace_id: ws,
            plan,
            validation_profile: None,
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
        std::process::exit(2);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_extract_function(
    root: PathBuf,
    source_file: String,
    start_line: u32,
    end_line: u32,
    new_name: String,
    params_csv: String,
    apply: bool,
) -> Result<()> {
    // Parse `--params name1:type1,name2:type2` into the wire shape
    // `[{ "name": "...", "type": "..." }, ...]`. Empty CSV means no
    // params (parameter-less helper).
    let mut params: Vec<serde_json::Value> = Vec::new();
    if !params_csv.trim().is_empty() {
        for (i, raw) in params_csv.split(',').enumerate() {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (name, ty) = trimmed.split_once(':').ok_or_else(|| {
                anyhow::anyhow!("params[{i}] must be in `name:type` form (got `{trimmed}`)")
            })?;
            params.push(serde_json::json!({
                "name": name.trim(),
                "type": ty.trim(),
            }));
        }
    }

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
        "op": "extract_function",
        "source_file": source_file,
        "start_line": start_line,
        "end_line": end_line,
        "new_name": new_name,
        "params": params,
        "files": [],
    });
    let plan = PatchPlanDto {
        ops: vec![op],
        label: format!("extract {source_file}:{start_line}..={end_line} -> {new_name}"),
    };

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
        "preview: {} byte-level edit(s) across {} file(s)",
        preview.total_replacements,
        preview.files.len()
    );
    for e in &preview.errors {
        eprintln!("  error in {}: {}", e.file, e.message);
    }
    for f in &preview.files {
        println!(
            "\n# {} ({} bytes -> {} bytes, {} edit(s))",
            f.path, f.before_len, f.after_len, f.replacements
        );
        println!("{}", f.diff);
    }

    if !apply {
        return Ok(());
    }
    if !preview.errors.is_empty() {
        println!("refusing to apply: preview reported errors");
        std::process::exit(2);
    }

    let resp = call(
        &core,
        METHOD_PATCH_APPLY,
        serde_json::to_value(PatchApplyParams {
            workspace_id: ws,
            plan,
            validation_profile: None,
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
        std::process::exit(2);
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
            client: aa_protocol::ClientCapabilities {
                name: "aa-cli".into(),
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
    match aa_rules_stub::parse(&src) {
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

fn cmd_history(
    root: PathBuf,
    label_prefix: Option<String>,
    op_tag: Option<String>,
    profile: Option<String>,
    limit: Option<usize>,
) -> Result<()> {
    let core = Core::new();
    let ws = open_workspace(&core, &root)?;
    let resp = call(
        &core,
        METHOD_MEMORY_HISTORY,
        serde_json::to_value(MemoryHistoryParams {
            workspace_id: ws,
            label_prefix,
            op_tag,
            validation_profile: profile,
            limit,
        })?,
    )?;
    let r: MemoryHistoryResult = serde_json::from_value(resp)?;
    if r.items.is_empty() {
        println!("(no commits)");
        return Ok(());
    }
    println!("{} commit(s), newest first:", r.items.len());
    for item in &r.items {
        let profile = item.validation_profile.as_deref().unwrap_or("-");
        let ops = if item.ops_summary.is_empty() {
            "(unknown)".to_string()
        } else {
            item.ops_summary.join(",")
        };
        println!(
            "  {}  ts={}  profile={}  ops=[{}]  files={}  repl={}  label={}",
            item.commit_id,
            item.timestamp_unix,
            profile,
            ops,
            item.files_changed,
            item.total_replacements,
            item.label,
        );
    }
    Ok(())
}

fn cmd_show(root: PathBuf, commit_id: String) -> Result<()> {
    let core = Core::new();
    let ws = open_workspace(&core, &root)?;
    let resp = call(
        &core,
        METHOD_MEMORY_GET,
        serde_json::to_value(MemoryGetParams {
            workspace_id: ws,
            commit_id: commit_id.clone(),
        })?,
    )?;
    let r: MemoryGetResult = serde_json::from_value(resp)?;
    let profile = r.validation_profile.as_deref().unwrap_or("-");
    println!(
        "commit {} @ ts={} profile={} ops=[{}] repl={} files={}",
        r.commit_id,
        r.timestamp_unix,
        profile,
        r.ops_summary.join(","),
        r.total_replacements,
        r.files.len(),
    );
    println!("label: {}", r.label);
    for f in &r.files {
        println!(
            "\n# {} ({} -> {} bytes)",
            f.path,
            f.before.len(),
            f.after.len()
        );
    }
    Ok(())
}

fn cmd_stats(root: PathBuf) -> Result<()> {
    let core = Core::new();
    let ws = open_workspace(&core, &root)?;
    let resp = call(
        &core,
        METHOD_MEMORY_STATS,
        serde_json::to_value(MemoryStatsParams { workspace_id: ws })?,
    )?;
    let r: MemoryStatsResult = serde_json::from_value(resp)?;
    println!(
        "commits: {}  files touched: {}  bytes written: {}",
        r.commits, r.files_touched, r.total_bytes_written
    );
    if let (Some(a), Some(b)) = (r.first_commit_at, r.last_commit_at) {
        println!("first: ts={}  last: ts={}", a, b);
    }
    if !r.by_op_kind.is_empty() {
        println!("by op kind:");
        for (k, v) in &r.by_op_kind {
            println!("  {k}: {v}");
        }
    }
    if !r.by_validation_profile.is_empty() {
        println!("by validation profile:");
        for (k, v) in &r.by_validation_profile {
            println!("  {k}: {v}");
        }
    }
    if !r.top_files.is_empty() {
        println!("top files (by edit count):");
        for f in &r.top_files {
            println!("  {} — {}", f.path, f.commit_count);
        }
    }
    Ok(())
}

/// Helper used by the memory subcommands — opens the workspace and
/// returns its id. Unlike the Datalog-centric `open` above, this
/// doesn't load a rule file; `aa history` / `show` / `stats` only
/// need the workspace to resolve the journal path.
fn open_workspace(core: &Core, root: &std::path::Path) -> Result<WorkspaceId> {
    let resp = call(
        core,
        METHOD_WORKSPACE_OPEN,
        serde_json::to_value(WorkspaceOpenParams {
            root: root.display().to_string(),
        })?,
    )?;
    Ok(serde_json::from_value::<WorkspaceOpenResult>(resp)?.workspace_id)
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

// Tiny wrapper so we don't pull aa-rules directly into the CLI dep graph
// beyond what we need for the `check` subcommand.
mod aa_rules_stub {
    pub fn parse(src: &str) -> Result<(usize, usize), String> {
        match aa_rules::parse(src) {
            Ok(p) => Ok((p.rules.len(), p.facts.len())),
            Err(e) => Err(e.to_string()),
        }
    }
}
