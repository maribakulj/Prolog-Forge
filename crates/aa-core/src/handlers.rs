//! Method dispatch table. Every arm decodes its typed params, calls the
//! corresponding operation, and encodes the typed result.

use aa_graph::{Fact, GraphStore, Pattern as GPattern, Term as GTerm};
use aa_protocol::*;
use aa_rules::{parse, Term};
use serde_json::{json, Value};

use crate::session::Core;

pub fn route(core: &Core, method: &str, params: Value) -> Result<Value, RpcError> {
    match method {
        METHOD_INITIALIZE => handle_initialize(params),
        METHOD_SHUTDOWN => Ok(json!(null)),
        METHOD_WORKSPACE_OPEN => handle_open(core, params),
        METHOD_WORKSPACE_STATUS => handle_status(core, params),
        METHOD_WORKSPACE_INDEX => handle_index(core, params),
        METHOD_GRAPH_INGEST => handle_ingest(core, params),
        METHOD_GRAPH_QUERY => handle_query(core, params),
        METHOD_RULES_LOAD => handle_rules_load(core, params),
        METHOD_RULES_EVALUATE => handle_rules_eval(core, params),
        METHOD_LLM_PROPOSE => handle_llm_propose(core, params),
        METHOD_LLM_REFINE => handle_llm_refine(core, params),
        METHOD_LLM_PROPOSE_PATCH => handle_llm_propose_patch(core, params),
        METHOD_PATCH_PREVIEW => handle_patch_preview(core, params),
        METHOD_PATCH_APPLY => handle_patch_apply(core, params),
        METHOD_PATCH_ROLLBACK => handle_patch_rollback(core, params),
        METHOD_EXPLAIN_PATCH => handle_explain_patch(core, params),
        METHOD_MEMORY_HISTORY => handle_memory_history(core, params),
        METHOD_MEMORY_GET => handle_memory_get(core, params),
        METHOD_MEMORY_STATS => handle_memory_stats(core, params),
        other => Err(RpcError::method_not_found(other)),
    }
}

fn decode<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
}

fn handle_initialize(params: Value) -> Result<Value, RpcError> {
    let _p: InitializeParams = decode(params)?;
    let caps = ServerCapabilities {
        name: "aye-aye".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PROTOCOL_VERSION.into(),
        methods: vec![
            METHOD_INITIALIZE.into(),
            METHOD_SHUTDOWN.into(),
            METHOD_WORKSPACE_OPEN.into(),
            METHOD_WORKSPACE_STATUS.into(),
            METHOD_WORKSPACE_INDEX.into(),
            METHOD_GRAPH_INGEST.into(),
            METHOD_GRAPH_QUERY.into(),
            METHOD_RULES_LOAD.into(),
            METHOD_RULES_EVALUATE.into(),
            METHOD_LLM_PROPOSE.into(),
            METHOD_LLM_REFINE.into(),
            METHOD_LLM_PROPOSE_PATCH.into(),
            METHOD_PATCH_PREVIEW.into(),
            METHOD_PATCH_APPLY.into(),
            METHOD_PATCH_ROLLBACK.into(),
            METHOD_EXPLAIN_PATCH.into(),
            METHOD_MEMORY_HISTORY.into(),
            METHOD_MEMORY_GET.into(),
            METHOD_MEMORY_STATS.into(),
        ],
    };
    Ok(serde_json::to_value(caps).unwrap())
}

fn handle_open(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: WorkspaceOpenParams = decode(params)?;
    let id = core.open(p.root);
    Ok(serde_json::to_value(WorkspaceOpenResult { workspace_id: id }).unwrap())
}

fn handle_index(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: WorkspaceIndexParams = decode(params)?;
    let report = core
        .with_workspace(&p.workspace_id, |ws, st| {
            crate::index::index_workspace(&ws.root, &mut st.graph)
        })
        .map_err(RpcError::invalid_params)?;
    Ok(serde_json::to_value(WorkspaceIndexResult {
        files_indexed: report.files_indexed,
        files_failed: report.files_failed,
        entities: report.entities,
        relations: report.relations,
        facts_inserted: report.facts_inserted,
        errors: report.errors,
    })
    .unwrap())
}

fn handle_status(core: &Core, params: Value) -> Result<Value, RpcError> {
    #[derive(serde::Deserialize)]
    struct P {
        workspace_id: WorkspaceId,
    }
    let p: P = decode(params)?;
    let status = core
        .with_workspace(&p.workspace_id, |ws, st| WorkspaceStatus {
            workspace_id: ws.id.clone(),
            root: ws.root.clone(),
            fact_count: st.graph.count_layer(FactLayer::Observed),
            rule_count: st.rules.len(),
            derived_count: st.graph.count_layer(FactLayer::Inferred),
        })
        .map_err(RpcError::invalid_params)?;
    Ok(serde_json::to_value(status).unwrap())
}

fn handle_ingest(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: IngestFactParams = decode(params)?;
    let result = core
        .with_workspace(&p.workspace_id, |_ws, st| {
            let mut inserted = 0;
            for dto in p.facts {
                let fact = Fact {
                    predicate: dto.predicate,
                    args: dto.args,
                    layer: dto.layer,
                };
                match st.graph.insert(fact) {
                    Ok(true) => inserted += 1,
                    Ok(false) => {}
                    Err(e) => return Err(RpcError::invalid_params(e.to_string())),
                }
            }
            Ok(IngestFactResult { inserted })
        })
        .map_err(RpcError::invalid_params)??;
    Ok(serde_json::to_value(result).unwrap())
}

fn handle_query(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: QueryParams = decode(params)?;
    // Parse the pattern by wrapping it as a ground clause "pat."
    let source = format!("{}.", p.pattern);
    let program = parse(&source).map_err(|e| RpcError::invalid_params(e.to_string()))?;
    if program.facts.len() + program.rules.len() != 1 {
        return Err(RpcError::invalid_params("query must be exactly one atom"));
    }
    let atom = if let Some(f) = program.facts.into_iter().next() {
        f
    } else {
        program.rules.into_iter().next().unwrap().head
    };
    let pattern = GPattern {
        predicate: atom.predicate,
        args: atom
            .args
            .into_iter()
            .map(|t| match t {
                Term::Const(c) => GTerm::Atom(c),
                Term::Var(v) => GTerm::Var(v),
            })
            .collect(),
    };
    let result = core
        .with_workspace(&p.workspace_id, |_ws, st| {
            collect_bindings(&pattern, &st.graph)
        })
        .map_err(RpcError::invalid_params)?;
    Ok(serde_json::to_value(result).unwrap())
}

fn collect_bindings(pattern: &GPattern, graph: &GraphStore) -> QueryResult {
    let bindings: Vec<Value> = pattern
        .matches(graph)
        .map(|b| serde_json::to_value(b).unwrap())
        .collect();
    QueryResult {
        count: bindings.len(),
        bindings,
    }
}

fn handle_rules_load(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: RulesLoadParams = decode(params)?;
    let program = parse(&p.source).map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let result = core
        .with_workspace(&p.workspace_id, |_ws, st| {
            let mut facts_added = 0;
            for a in program.facts {
                let args: Vec<String> = a
                    .args
                    .into_iter()
                    .map(|t| match t {
                        Term::Const(c) => c,
                        Term::Var(v) => v, // parser guarantees facts have no vars
                    })
                    .collect();
                if st
                    .graph
                    .insert(Fact {
                        predicate: a.predicate,
                        args,
                        layer: FactLayer::Observed,
                    })
                    .map_err(|e| RpcError::invalid_params(e.to_string()))?
                {
                    facts_added += 1;
                }
            }
            let rules_added = program.rules.len();
            st.rules.extend(program.rules);
            Ok::<_, RpcError>(RulesLoadResult {
                rules_added,
                facts_added,
            })
        })
        .map_err(RpcError::invalid_params)??;
    Ok(serde_json::to_value(result).unwrap())
}

fn handle_patch_preview(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: PatchPreviewParams = decode(params)?;
    // Decode ops from tagged JSON objects into typed PatchOps.
    let mut ops: Vec<aa_patch::PatchOp> = Vec::with_capacity(p.plan.ops.len());
    for raw in p.plan.ops {
        let op: aa_patch::PatchOp = serde_json::from_value(raw)
            .map_err(|e| RpcError::invalid_params(format!("bad op: {e}")))?;
        ops.push(op);
    }
    let plan = aa_patch::PatchPlan::labelled(ops, p.plan.label);

    // Load source texts from the workspace root (Rust files only for now).
    let (root, files) = core
        .with_workspace(&p.workspace_id, |ws, _st| {
            let root = std::path::PathBuf::from(&ws.root);
            let mut map: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            for sf in aa_ingest::walk(&root, &aa_ingest::IngestOptions::default()) {
                if sf.language != "rust" {
                    continue;
                }
                if let Ok(src) = std::fs::read_to_string(&sf.path) {
                    map.insert(sf.relative.display().to_string(), src);
                }
            }
            (root, map)
        })
        .map_err(RpcError::invalid_params)?;

    let session_key = root.display().to_string();
    let resolver = crate::ra_pool::PooledResolver {
        pool: &core.ra_pool,
        session_key,
    };
    let preview = aa_patch::preview_with_resolver(&plan, &files, &resolver)
        .map_err(|e| RpcError::internal(e.to_string()))?;

    Ok(serde_json::to_value(PatchPreviewResult {
        total_replacements: preview.total_replacements,
        files: preview
            .files
            .into_iter()
            .map(|f| FilePatchDto {
                path: f.path,
                before_len: f.before_len,
                after_len: f.after_len,
                replacements: f.replacements,
                diff: f.diff,
            })
            .collect(),
        errors: preview
            .errors
            .into_iter()
            .map(|e| FilePatchError {
                file: e.file,
                message: e.message,
            })
            .collect(),
    })
    .unwrap())
}

fn handle_patch_apply(core: &Core, params: Value) -> Result<Value, RpcError> {
    use aa_validate::{Pipeline, ValidationContext, ValidationStage};

    let p: PatchApplyParams = decode(params)?;
    let profile = p.validation_profile.as_deref().unwrap_or("default");

    // Decode plan (shared shape with preview).
    let mut ops: Vec<aa_patch::PatchOp> = Vec::with_capacity(p.plan.ops.len());
    for raw in p.plan.ops {
        let op: aa_patch::PatchOp = serde_json::from_value(raw)
            .map_err(|e| RpcError::invalid_params(format!("bad op: {e}")))?;
        ops.push(op);
    }
    let plan = aa_patch::PatchPlan::labelled(ops, p.plan.label);
    let plan_label = plan.label.clone();

    // Load originals + root + snapshot the workspace's rule set.
    let (root, original, rules) = core
        .with_workspace(&p.workspace_id, |w, st| {
            let root = std::path::PathBuf::from(&w.root);
            let mut map: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            for sf in aa_ingest::walk(&root, &aa_ingest::IngestOptions::default()) {
                if sf.language != "rust" {
                    continue;
                }
                if let Ok(src) = std::fs::read_to_string(&sf.path) {
                    map.insert(sf.relative.display().to_string(), src);
                }
            }
            (root, map, st.rules.clone())
        })
        .map_err(RpcError::invalid_params)?;

    // Build the shadow file map by re-applying each op. Typed-rename
    // ops route through the Core's persistent RA session pool so
    // back-to-back previews + applies reuse the same warm session.
    let session_key = root.display().to_string();
    let shadow = build_shadow(core, &session_key, &plan, &original);

    // Diff-based replacement count (for parity with preview's display).
    let total_replacements = shadow
        .iter()
        .filter(|(k, v)| original.get(*k).map(|o| o != *v).unwrap_or(false))
        .count();

    // Build the pipeline. SyntacticStage always runs; RuleStage runs only
    // when the workspace has rules loaded — rule packs gate applies via
    // the `violation(...)` convention documented in docs/rules-dsl.md.
    // CargoCheckStage is opt-in via `validation_profile = "typed"`;
    // CargoTestStage via `= "tested"` and narrows its run via the
    // Phase 1.16 impacted-tests selector when anchors are available.
    let anchors = anchors_from_ops(&plan.ops);
    let impacted_tests = if profile == "tested" {
        crate::test_impact::impacted_test_names(&original, &anchors)
    } else {
        Vec::new()
    };
    let stages: Vec<Box<dyn ValidationStage>> =
        build_pipeline(profile, &root, &rules, impacted_tests).map_err(RpcError::invalid_params)?;
    let validation = Pipeline::custom(stages).run(&ValidationContext {
        shadow_files: &shadow,
        original_files: &original,
    });
    let validation_dto = to_validation_dto(&validation);

    if !validation.ok {
        return Ok(serde_json::to_value(PatchApplyResult {
            applied: false,
            commit_id: None,
            files_written: 0,
            bytes_written: 0,
            total_replacements,
            validation: validation_dto,
            rejection_reason: Some("validation failed".into()),
        })
        .unwrap());
    }

    match crate::apply::apply_transactional(&root, &shadow, &original) {
        Ok(out) => {
            // Record the commit to the on-disk journal so `patch.rollback`
            // can undo it. Journal failures are non-fatal for the apply
            // itself — surface them as warnings on stderr via tracing but
            // still report the commit as applied.
            let files: Vec<crate::journal::CommitFile> = shadow
                .iter()
                .filter_map(|(rel, new_content)| {
                    let before = original.get(rel)?;
                    if before == new_content {
                        None
                    } else {
                        Some(crate::journal::CommitFile {
                            path: rel.clone(),
                            before: before.clone(),
                            after: new_content.clone(),
                        })
                    }
                })
                .collect();
            // Phase 1.14: record op tags + profile + replacement
            // count so `memory.stats` can aggregate without parsing
            // the free-text `label`.
            let ops_summary: Vec<String> = plan.ops.iter().map(op_tag).collect();
            let entry = crate::journal::new_entry_with_stats(
                out.commit_id.clone(),
                plan_label,
                files,
                ops_summary,
                Some(profile.to_string()),
                total_replacements,
            );
            if let Err(e) = crate::journal::write(&root, &entry) {
                tracing::warn!("commit journal write failed: {e}");
            }

            Ok(serde_json::to_value(PatchApplyResult {
                applied: true,
                commit_id: Some(out.commit_id),
                files_written: out.files_written,
                bytes_written: out.bytes_written,
                total_replacements,
                validation: validation_dto,
                rejection_reason: None,
            })
            .unwrap())
        }
        Err(e) => Ok(serde_json::to_value(PatchApplyResult {
            applied: false,
            commit_id: None,
            files_written: 0,
            bytes_written: 0,
            total_replacements,
            validation: validation_dto,
            rejection_reason: Some(e.to_string()),
        })
        .unwrap()),
    }
}

fn handle_patch_rollback(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: PatchRollbackParams = decode(params)?;
    let root = core
        .with_workspace(&p.workspace_id, |w, _st| std::path::PathBuf::from(&w.root))
        .map_err(RpcError::invalid_params)?;

    match crate::rollback::rollback(&root, &p.commit_id) {
        Ok(out) => Ok(serde_json::to_value(PatchRollbackResult {
            rolled_back: true,
            commit_id: out.commit_id,
            files_restored: out.files_restored,
            label: out.label,
            reason: None,
        })
        .unwrap()),
        Err(e) => Ok(serde_json::to_value(PatchRollbackResult {
            rolled_back: false,
            commit_id: p.commit_id,
            files_restored: 0,
            label: String::new(),
            reason: Some(e.to_string()),
        })
        .unwrap()),
    }
}

/// Compose a validation pipeline from the (named) `validation_profile`.
///
/// Known profiles:
/// - `"default"` / missing: `SyntacticStage`, then `RuleStage` when the
///   workspace has rules loaded. This is the cheap, always-on pipeline.
/// - `"typed"`: everything in `"default"` plus `CargoCheckStage`, which
///   materialises the shadow files in a temp directory and shells out to
///   `cargo check`. `cargo` must be on `PATH`; the stage passes with a
///   warning diagnostic when it isn't.
/// - `"tested"`: everything in `"typed"` plus `CargoTestStage`, which
///   additionally runs `cargo test` against the shadow. Strongest
///   verdict but correspondingly slower; intended for CI-grade
///   `patch.apply`.
fn build_pipeline(
    profile: &str,
    workspace_root: &std::path::Path,
    rules: &[aa_rules::Rule],
    impacted_tests: Vec<String>,
) -> Result<Vec<Box<dyn aa_validate::ValidationStage>>, String> {
    let mut stages: Vec<Box<dyn aa_validate::ValidationStage>> =
        vec![Box::new(aa_validate::SyntacticStage)];
    if !rules.is_empty() {
        stages.push(Box::new(crate::validate_stages::RuleStage::new(
            rules.to_vec(),
        )));
    }
    match profile {
        "default" | "" => {}
        "typed" => {
            stages.push(Box::new(crate::validate_stages::CargoCheckStage::new(
                workspace_root.to_path_buf(),
                std::time::Duration::from_secs(180),
            )));
        }
        "tested" => {
            stages.push(Box::new(crate::validate_stages::CargoCheckStage::new(
                workspace_root.to_path_buf(),
                std::time::Duration::from_secs(180),
            )));
            // Phase 1.16: if the caller pre-computed a non-empty
            // impacted-test list from the plan's anchors, pass it to
            // `cargo test` as a substring filter. Empty list means
            // "run all" — safer default than guessing.
            stages.push(Box::new(
                crate::validate_stages::CargoTestStage::new(
                    workspace_root.to_path_buf(),
                    std::time::Duration::from_secs(300),
                )
                .with_selection(impacted_tests),
            ));
        }
        other => {
            return Err(format!(
                "unknown validation_profile `{other}` (known: default, typed, tested)"
            ));
        }
    }
    Ok(stages)
}

/// Thin wrapper around `aa_patch::apply_plan_with_resolver` so the
/// `apply` / `explain` / `preview` handlers share the same single
/// op-dispatch path. `session_key` identifies the workspace for the
/// RA session pool; passing it in (rather than pulling it out of the
/// plan) keeps `aa-patch` oblivious to our pool implementation.
fn build_shadow(
    core: &Core,
    session_key: &str,
    plan: &aa_patch::PatchPlan,
    original: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let resolver = crate::ra_pool::PooledResolver {
        pool: &core.ra_pool,
        session_key: session_key.to_string(),
    };
    let (working, _errors) = aa_patch::apply_plan_with_resolver(plan, original, &resolver);
    working
}

fn to_validation_dto(r: &aa_validate::ValidationReport) -> ValidationReportDto {
    ValidationReportDto {
        ok: r.ok,
        stages: r
            .stages
            .iter()
            .map(|s| StageReportDto {
                stage: s.stage.clone(),
                ok: s.ok,
                diagnostics: s
                    .diagnostics
                    .iter()
                    .map(|d| DiagnosticDto {
                        severity: match d.severity {
                            aa_validate::Severity::Error => "error".into(),
                            aa_validate::Severity::Warning => "warning".into(),
                            aa_validate::Severity::Info => "info".into(),
                        },
                        file: d.file.clone(),
                        message: d.message.clone(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn handle_llm_propose(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: LlmProposeParams = decode(params)?;
    let result = core
        .with_workspace(&p.workspace_id, |_ws, st| {
            aa_llm::propose(
                core.llm_provider.as_ref(),
                &core.llm_cache,
                &mut st.graph,
                aa_llm::ProposeRequest {
                    intent: &p.intent,
                    anchor_id: &p.anchor_id,
                    hops: p.hops,
                    max_facts: p.max_facts,
                    max_tokens: 1024,
                    temperature: 0.0,
                },
            )
            .map_err(|e| RpcError::internal(e.to_string()))
        })
        .map_err(RpcError::invalid_params)??;
    Ok(serde_json::to_value(LlmProposeResult {
        accepted: result.accepted,
        rejected: result.rejected,
        cache_hit: result.cache_hit,
        tokens_in: result.tokens_in,
        tokens_out: result.tokens_out,
        outcomes: result
            .outcomes
            .into_iter()
            .map(|o| ProposalOutcomeDto {
                predicate: o.predicate,
                args: o.args,
                justification: o.justification,
                accepted: o.accepted,
                rejection_reason: o.rejection_reason,
                round: Some(0),
            })
            .collect(),
    })
    .unwrap())
}

fn handle_llm_refine(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: LlmRefineParams = decode(params)?;

    let prior_outcomes: Vec<aa_llm::ProposalOutcome> = p
        .prior_outcomes
        .into_iter()
        .map(|o| aa_llm::ProposalOutcome {
            predicate: o.predicate,
            args: o.args,
            justification: o.justification,
            accepted: o.accepted,
            rejection_reason: o.rejection_reason,
        })
        .collect();
    let prior_diagnostics: Vec<aa_llm::RefinerDiagnostic> = p
        .prior_diagnostics
        .into_iter()
        .map(|d| aa_llm::RefinerDiagnostic {
            severity: d.severity,
            file: d.file,
            message: d.message,
        })
        .collect();

    let result = core
        .with_workspace(&p.workspace_id, |_ws, st| {
            aa_llm::refine(
                core.llm_provider.as_ref(),
                &core.llm_cache,
                &mut st.graph,
                aa_llm::RefineRequest {
                    intent: &p.intent,
                    anchor_id: &p.anchor_id,
                    hops: p.hops,
                    max_facts: p.max_facts,
                    max_rounds: p.max_rounds,
                    max_tokens: 1024,
                    temperature: 0.0,
                    prior_outcomes,
                    prior_diagnostics,
                },
            )
            .map_err(|e| RpcError::internal(e.to_string()))
        })
        .map_err(RpcError::invalid_params)??;

    // Re-annotate per-round so the wire payload tells callers which
    // hypotheses came from which iteration. The loop already emits
    // `rounds_summary[i].{accepted,rejected}` which totals to the same
    // counts; we walk the outcome list in that order.
    let mut annotated = Vec::with_capacity(result.outcomes.len());
    let mut iter = result.outcomes.into_iter();
    for rs in &result.rounds_summary {
        let take = rs.accepted + rs.rejected;
        for _ in 0..take {
            if let Some(o) = iter.next() {
                annotated.push(ProposalOutcomeDto {
                    predicate: o.predicate,
                    args: o.args,
                    justification: o.justification,
                    accepted: o.accepted,
                    rejection_reason: o.rejection_reason,
                    round: Some(rs.round),
                });
            }
        }
    }
    // Drain any leftover (should not happen, but keep the data).
    for o in iter {
        annotated.push(ProposalOutcomeDto {
            predicate: o.predicate,
            args: o.args,
            justification: o.justification,
            accepted: o.accepted,
            rejection_reason: o.rejection_reason,
            round: None,
        });
    }

    Ok(serde_json::to_value(LlmRefineResult {
        rounds: result.rounds,
        converged: result.converged,
        final_accepted: result.final_accepted,
        final_rejected: result.final_rejected,
        tokens_in_total: result.tokens_in_total,
        tokens_out_total: result.tokens_out_total,
        outcomes: annotated,
        rounds_summary: result
            .rounds_summary
            .into_iter()
            .map(|s| RefineRoundSummary {
                round: s.round,
                accepted: s.accepted,
                rejected: s.rejected,
                cache_hit: s.cache_hit,
                tokens_in: s.tokens_in,
                tokens_out: s.tokens_out,
            })
            .collect(),
    })
    .unwrap())
}

fn handle_llm_propose_patch(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: LlmProposePatchParams = decode(params)?;

    // Phase 1.15: if the caller asks for memory-aware proposal, read
    // the top-N recent commits into a `Vec<CommitEntry>` up-front
    // (outside the workspace-state lock, since it's fs I/O) and keep
    // them alive for the duration of the call. The `MemoryHint`
    // borrows from this vec.
    let memory_entries: Vec<crate::journal::CommitEntry> = match p.include_memory {
        Some(n) if n > 0 => {
            let root = core
                .with_workspace(&p.workspace_id, |w, _st| std::path::PathBuf::from(&w.root))
                .map_err(RpcError::invalid_params)?;
            let items = crate::memory::history(
                &root,
                &crate::memory::HistoryFilter {
                    limit: Some(n),
                    ..Default::default()
                },
            )
            .map_err(|e| RpcError::internal(e.to_string()))?;
            items
                .into_iter()
                .filter_map(|it| crate::memory::get(&root, &it.commit_id).ok())
                .collect()
        }
        _ => Vec::new(),
    };
    let hints: Vec<aa_llm::MemoryHint<'_>> = memory_entries
        .iter()
        .map(|e| aa_llm::MemoryHint {
            label: &e.label,
            ops_summary: &e.ops_summary,
            validation_profile: e.validation_profile.as_deref(),
            total_replacements: e.total_replacements,
        })
        .collect();

    let result = core
        .with_workspace(&p.workspace_id, |_ws, st| {
            aa_llm::propose_patch(
                core.llm_provider.as_ref(),
                &core.llm_cache,
                &st.graph,
                aa_llm::ProposePatchRequest {
                    intent: &p.intent,
                    anchor_id: &p.anchor_id,
                    hops: p.hops,
                    max_facts: p.max_facts,
                    max_tokens: 1024,
                    temperature: 0.0,
                    memory_hints: hints,
                },
            )
            .map_err(|e| RpcError::internal(e.to_string()))
        })
        .map_err(RpcError::invalid_params)??;
    Ok(serde_json::to_value(LlmProposePatchResult {
        accepted: result.accepted,
        rejected: result.rejected,
        cache_hit: result.cache_hit,
        tokens_in: result.tokens_in,
        tokens_out: result.tokens_out,
        candidates: result
            .candidates
            .into_iter()
            .map(|c| PatchCandidateDto {
                plan: PatchPlanDto {
                    ops: c.plan.ops,
                    label: c.plan.label,
                },
                justification: c.justification,
                accepted: c.accepted,
                rejection_reason: c.rejection_reason,
            })
            .collect(),
    })
    .unwrap())
}

fn handle_explain_patch(core: &Core, params: Value) -> Result<Value, RpcError> {
    use aa_validate::{Pipeline, ValidationContext, ValidationStage};

    let p: ExplainPatchParams = decode(params)?;

    // Decode the plan the same way patch.preview/apply do.
    let mut ops: Vec<aa_patch::PatchOp> = Vec::with_capacity(p.plan.ops.len());
    for raw in &p.plan.ops {
        let op: aa_patch::PatchOp = serde_json::from_value(raw.clone())
            .map_err(|e| RpcError::invalid_params(format!("bad op: {e}")))?;
        ops.push(op);
    }
    let plan = aa_patch::PatchPlan::labelled(ops.clone(), p.plan.label.clone());
    let anchors: Vec<String> = anchors_from_ops(&ops);

    // Load originals + rules snapshot.
    let (root, original, rules) = core
        .with_workspace(&p.workspace_id, |w, st| {
            let root = std::path::PathBuf::from(&w.root);
            let mut map: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            for sf in aa_ingest::walk(&root, &aa_ingest::IngestOptions::default()) {
                if sf.language != "rust" {
                    continue;
                }
                if let Ok(src) = std::fs::read_to_string(&sf.path) {
                    map.insert(sf.relative.display().to_string(), src);
                }
            }
            (root, map, st.rules.clone())
        })
        .map_err(RpcError::invalid_params)?;

    // Build shadow + run validation (same pipeline as patch.apply, but no
    // transactional write). `validation_profile` selects which stages run;
    // the default omits CargoCheckStage so `explain.patch` stays cheap.
    let session_key = root.display().to_string();
    let shadow = build_shadow(core, &session_key, &plan, &original);
    let profile = p.validation_profile.as_deref().unwrap_or("default");
    let impacted_tests = if profile == "tested" {
        crate::test_impact::impacted_test_names(&original, &anchors)
    } else {
        Vec::new()
    };
    let stages: Vec<Box<dyn ValidationStage>> =
        build_pipeline(profile, &root, &rules, impacted_tests).map_err(RpcError::invalid_params)?;
    let validation = Pipeline::custom(stages).run(&ValidationContext {
        shadow_files: &shadow,
        original_files: &original,
    });

    // Adapt the validation report into the explainer's stage view.
    let stage_refs: Vec<aa_explain::ValidationStageRef> = validation
        .stages
        .iter()
        .map(|s| aa_explain::ValidationStageRef {
            name: s.stage.clone(),
            ok: s.ok,
            diagnostics: s
                .diagnostics
                .iter()
                .map(|d| aa_explain::model::StageDiagnostic {
                    severity: match d.severity {
                        aa_validate::Severity::Error => "error".into(),
                        aa_validate::Severity::Warning => "warning".into(),
                        aa_validate::Severity::Info => "info".into(),
                    },
                    file: d.file.clone(),
                    message: d.message.clone(),
                })
                .collect(),
        })
        .collect();

    // Candidate outcomes passed by the caller (may be empty).
    let candidate_refs: Vec<aa_explain::CandidateRef> = p
        .candidate_outcomes
        .iter()
        .map(|o| aa_explain::CandidateRef {
            predicate: o.predicate.clone(),
            args: o.args.clone(),
            justification: o.justification.clone(),
            accepted: o.accepted,
            rejection_reason: o.rejection_reason.clone(),
            round: o.round,
        })
        .collect();

    // Build the explanation under a read-lock on the workspace so the
    // graph snapshot is coherent with the rules we ran.
    let explanation = core
        .with_workspace(&p.workspace_id, |_ws, st| {
            let rejection_reason = if validation.ok {
                None
            } else {
                Some("validation failed".to_string())
            };
            aa_explain::build(aa_explain::ExplainInput {
                plan_label: &p.plan.label,
                anchors: &anchors,
                graph: &st.graph,
                rules: &rules,
                candidate_outcomes: &candidate_refs,
                validation_stages: &stage_refs,
                commit_id: None,
                rejection_reason,
            })
        })
        .map_err(RpcError::invalid_params)?;

    Ok(serde_json::to_value(explanation_to_dto(explanation)).unwrap())
}

fn handle_memory_history(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: MemoryHistoryParams = decode(params)?;
    let root = core
        .with_workspace(&p.workspace_id, |w, _st| std::path::PathBuf::from(&w.root))
        .map_err(RpcError::invalid_params)?;
    let filter = crate::memory::HistoryFilter {
        label_prefix: p.label_prefix,
        op_tag: p.op_tag,
        validation_profile: p.validation_profile,
        limit: p.limit,
    };
    let items =
        crate::memory::history(&root, &filter).map_err(|e| RpcError::internal(e.to_string()))?;
    let dto = MemoryHistoryResult {
        items: items
            .into_iter()
            .map(|it| MemoryHistoryItemDto {
                commit_id: it.commit_id,
                timestamp_unix: it.timestamp_unix,
                label: it.label,
                files_changed: it.files_changed,
                bytes_after: it.bytes_after,
                ops_summary: it.ops_summary,
                validation_profile: it.validation_profile,
                total_replacements: it.total_replacements,
            })
            .collect(),
    };
    Ok(serde_json::to_value(dto).unwrap())
}

fn handle_memory_get(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: MemoryGetParams = decode(params)?;
    let root = core
        .with_workspace(&p.workspace_id, |w, _st| std::path::PathBuf::from(&w.root))
        .map_err(RpcError::invalid_params)?;
    let entry = crate::memory::get(&root, &p.commit_id).map_err(|e| match e {
        crate::journal::JournalError::NotFound(id) => {
            RpcError::invalid_params(format!("commit not found: {id}"))
        }
        other => RpcError::internal(other.to_string()),
    })?;
    let dto = MemoryGetResult {
        commit_id: entry.commit_id,
        timestamp_unix: entry.timestamp_unix,
        label: entry.label,
        ops_summary: entry.ops_summary,
        validation_profile: entry.validation_profile,
        total_replacements: entry.total_replacements,
        files: entry
            .files
            .into_iter()
            .map(|f| CommitFileDto {
                path: f.path,
                before: f.before,
                after: f.after,
            })
            .collect(),
    };
    Ok(serde_json::to_value(dto).unwrap())
}

fn handle_memory_stats(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: MemoryStatsParams = decode(params)?;
    let root = core
        .with_workspace(&p.workspace_id, |w, _st| std::path::PathBuf::from(&w.root))
        .map_err(RpcError::invalid_params)?;
    let s = crate::memory::stats(&root).map_err(|e| RpcError::internal(e.to_string()))?;
    let dto = MemoryStatsResult {
        commits: s.commits,
        files_touched: s.files_touched,
        by_op_kind: s.by_op_kind,
        by_validation_profile: s.by_validation_profile,
        top_files: s
            .top_files
            .into_iter()
            .map(|(path, commit_count)| MemoryTopFileDto { path, commit_count })
            .collect(),
        first_commit_at: s.first_commit_at,
        last_commit_at: s.last_commit_at,
        total_bytes_written: s.total_bytes_written,
    };
    Ok(serde_json::to_value(dto).unwrap())
}

/// Canonical tag string for a patch op, matching the wire `op` field
/// used in `PatchPlanDto::ops`. Shared between the journal writer
/// (stats / memory surface) and future consumers that need to group by
/// op kind without round-tripping through JSON.
fn op_tag(op: &aa_patch::PatchOp) -> String {
    match op {
        aa_patch::PatchOp::RenameFunction { .. } => "rename_function".into(),
        aa_patch::PatchOp::RenameFunctionTyped { .. } => "rename_function_typed".into(),
        aa_patch::PatchOp::AddDeriveToStruct { .. } => "add_derive_to_struct".into(),
        aa_patch::PatchOp::RemoveDeriveFromStruct { .. } => "remove_derive_from_struct".into(),
        aa_patch::PatchOp::InlineFunction { .. } => "inline_function".into(),
    }
}

fn anchors_from_ops(ops: &[aa_patch::PatchOp]) -> Vec<String> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            aa_patch::PatchOp::RenameFunction {
                old_name, new_name, ..
            } => {
                out.push(old_name.clone());
                out.push(new_name.clone());
            }
            aa_patch::PatchOp::RenameFunctionTyped {
                old_name, new_name, ..
            } => {
                // `old_name` is informative on the typed variant (RA
                // resolves by position) but we still emit it as an
                // anchor so the explainer surfaces the same observed
                // facts it would for the syntactic rename.
                if !old_name.is_empty() {
                    out.push(old_name.clone());
                }
                out.push(new_name.clone());
            }
            aa_patch::PatchOp::AddDeriveToStruct {
                type_name, derives, ..
            } => {
                // The type name is the primary anchor — the explainer
                // uses it to surface the `struct_def(_, type_name)`
                // observed fact. The derive names themselves are
                // anchors too: in a future phase a rule pack might
                // emit `violation(X) :- has_derive(X, "Unsafe")` and
                // the explainer needs to see those.
                out.push(type_name.clone());
                out.extend(derives.iter().cloned());
            }
            aa_patch::PatchOp::RemoveDeriveFromStruct {
                type_name, derives, ..
            } => {
                // Symmetric to AddDeriveToStruct: same anchors, same
                // explainer surface. A rule pack that asserts
                // `violation(X) :- requires_derive(X, "Serialize")`
                // will see its premise fact cited exactly the same way.
                out.push(type_name.clone());
                out.extend(derives.iter().cloned());
            }
            aa_patch::PatchOp::InlineFunction { function, .. } => {
                // The inlined function is the one and only anchor: the
                // explainer surfaces `function(_, function)` and any
                // `calls(_, <fn_id>)` relations citing it.
                out.push(function.clone());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn explanation_to_dto(e: aa_explain::Explanation) -> ExplainPatchResult {
    ExplainPatchResult {
        plan_label: e.plan_label,
        anchors: e.anchors,
        verdict: match e.verdict {
            aa_explain::Verdict::Accepted { commit_id, notes } => {
                VerdictDto::Accepted { commit_id, notes }
            }
            aa_explain::Verdict::Rejected {
                reason,
                failing_stages,
            } => VerdictDto::Rejected {
                reason,
                failing_stages,
            },
            aa_explain::Verdict::NotProven { reason } => VerdictDto::NotProven { reason },
        },
        evidence: e.evidence.into_iter().map(evidence_to_dto).collect(),
        stats: ExplanationStatsDto {
            anchors: e.stats.anchors,
            observed_cited: e.stats.observed_cited,
            inferred_cited: e.stats.inferred_cited,
            rule_activations: e.stats.rule_activations,
            candidates_considered: e.stats.candidates_considered,
            stages_run: e.stats.stages_run,
        },
        summary: e.summary,
    }
}

fn evidence_to_dto(n: aa_explain::EvidenceNode) -> EvidenceNodeDto {
    match n {
        aa_explain::EvidenceNode::Observed {
            predicate,
            args,
            role,
        } => EvidenceNodeDto::Observed {
            predicate,
            args,
            role,
        },
        aa_explain::EvidenceNode::Inferred { predicate, args } => {
            EvidenceNodeDto::Inferred { predicate, args }
        }
        aa_explain::EvidenceNode::RuleActivation {
            rule_index,
            head,
            premises,
        } => EvidenceNodeDto::RuleActivation {
            rule_index,
            head: PremiseFactDto {
                predicate: head.predicate,
                args: head.args,
                layer: head.layer,
            },
            premises: premises
                .into_iter()
                .map(|p| PremiseFactDto {
                    predicate: p.predicate,
                    args: p.args,
                    layer: p.layer,
                })
                .collect(),
        },
        aa_explain::EvidenceNode::Candidate(c) => EvidenceNodeDto::Candidate {
            predicate: c.predicate,
            args: c.args,
            justification: c.justification,
            accepted: c.accepted,
            rejection_reason: c.rejection_reason,
            round: c.round,
        },
        aa_explain::EvidenceNode::Stage {
            name,
            ok,
            diagnostics,
        } => EvidenceNodeDto::Stage {
            name,
            ok,
            diagnostics: diagnostics
                .into_iter()
                .map(|d| DiagnosticDto {
                    severity: d.severity,
                    file: d.file,
                    message: d.message,
                })
                .collect(),
        },
    }
}

fn handle_rules_eval(core: &Core, params: Value) -> Result<Value, RpcError> {
    let p: RulesEvaluateParams = decode(params)?;
    let stats = core
        .with_workspace(&p.workspace_id, |_ws, st| {
            aa_rules::evaluate(&st.rules, &mut st.graph)
                .map_err(|e| RpcError::internal(e.to_string()))
        })
        .map_err(RpcError::invalid_params)??;
    Ok(serde_json::to_value(RulesEvaluateResult {
        derived: stats.derived,
        iterations: stats.iterations,
    })
    .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_protocol::{Id, Request};

    fn req(method: &str, params: Value) -> Request {
        Request {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params: Some(params),
            id: Some(Id::Num(1)),
        }
    }

    #[test]
    fn end_to_end_ancestor() {
        let core = Core::new();
        // open workspace
        let r = crate::dispatch(&core, req(METHOD_WORKSPACE_OPEN, json!({"root":"/tmp"}))).unwrap();
        let open: WorkspaceOpenResult = serde_json::from_value(r.result.unwrap()).unwrap();

        // load rules + facts
        let rules_src = r#"
            parent(alice, bob).
            parent(bob, carol).
            ancestor(X,Y) :- parent(X,Y).
            ancestor(X,Z) :- parent(X,Y), ancestor(Y,Z).
        "#;
        let r = crate::dispatch(
            &core,
            req(
                METHOD_RULES_LOAD,
                json!({"workspace_id": open.workspace_id, "source": rules_src}),
            ),
        )
        .unwrap();
        let _: RulesLoadResult = serde_json::from_value(r.result.unwrap()).unwrap();

        // evaluate
        let r = crate::dispatch(
            &core,
            req(
                METHOD_RULES_EVALUATE,
                json!({"workspace_id": open.workspace_id}),
            ),
        )
        .unwrap();
        let stats: RulesEvaluateResult = serde_json::from_value(r.result.unwrap()).unwrap();
        assert_eq!(stats.derived, 3); // 2 direct + 1 transitive

        // query
        let r = crate::dispatch(
            &core,
            req(
                METHOD_GRAPH_QUERY,
                json!({"workspace_id": open.workspace_id, "pattern": "ancestor(alice, X)"}),
            ),
        )
        .unwrap();
        let qr: QueryResult = serde_json::from_value(r.result.unwrap()).unwrap();
        assert_eq!(qr.count, 2);
    }
}
