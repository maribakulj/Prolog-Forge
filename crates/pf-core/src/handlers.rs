//! Method dispatch table. Every arm decodes its typed params, calls the
//! corresponding operation, and encodes the typed result.

use pf_graph::{Fact, GraphStore, Pattern as GPattern, Term as GTerm};
use pf_protocol::*;
use pf_rules::{parse, Term};
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
        METHOD_PATCH_PREVIEW => handle_patch_preview(core, params),
        METHOD_PATCH_APPLY => handle_patch_apply(core, params),
        METHOD_PATCH_ROLLBACK => handle_patch_rollback(core, params),
        METHOD_EXPLAIN_PATCH => handle_explain_patch(core, params),
        other => Err(RpcError::method_not_found(other)),
    }
}

fn decode<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
}

fn handle_initialize(params: Value) -> Result<Value, RpcError> {
    let _p: InitializeParams = decode(params)?;
    let caps = ServerCapabilities {
        name: "prolog-forge".into(),
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
            METHOD_PATCH_PREVIEW.into(),
            METHOD_PATCH_APPLY.into(),
            METHOD_PATCH_ROLLBACK.into(),
            METHOD_EXPLAIN_PATCH.into(),
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
    let mut ops: Vec<pf_patch::PatchOp> = Vec::with_capacity(p.plan.ops.len());
    for raw in p.plan.ops {
        let op: pf_patch::PatchOp = serde_json::from_value(raw)
            .map_err(|e| RpcError::invalid_params(format!("bad op: {e}")))?;
        ops.push(op);
    }
    let plan = pf_patch::PatchPlan::labelled(ops, p.plan.label);

    // Load source texts from the workspace root (Rust files only for now).
    let files = core
        .with_workspace(&p.workspace_id, |ws, _st| {
            let root = std::path::PathBuf::from(&ws.root);
            let mut map: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            for sf in pf_ingest::walk(&root, &pf_ingest::IngestOptions::default()) {
                if sf.language != "rust" {
                    continue;
                }
                if let Ok(src) = std::fs::read_to_string(&sf.path) {
                    map.insert(sf.relative.display().to_string(), src);
                }
            }
            map
        })
        .map_err(RpcError::invalid_params)?;

    let preview =
        pf_patch::preview(&plan, &files).map_err(|e| RpcError::internal(e.to_string()))?;

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
    use pf_validate::{Pipeline, ValidationContext, ValidationStage};

    let p: PatchApplyParams = decode(params)?;
    let profile = p.validation_profile.as_deref().unwrap_or("default");

    // Decode plan (shared shape with preview).
    let mut ops: Vec<pf_patch::PatchOp> = Vec::with_capacity(p.plan.ops.len());
    for raw in p.plan.ops {
        let op: pf_patch::PatchOp = serde_json::from_value(raw)
            .map_err(|e| RpcError::invalid_params(format!("bad op: {e}")))?;
        ops.push(op);
    }
    let plan = pf_patch::PatchPlan::labelled(ops, p.plan.label);
    let plan_label = plan.label.clone();

    // Load originals + root + snapshot the workspace's rule set.
    let (root, original, rules) = core
        .with_workspace(&p.workspace_id, |w, st| {
            let root = std::path::PathBuf::from(&w.root);
            let mut map: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            for sf in pf_ingest::walk(&root, &pf_ingest::IngestOptions::default()) {
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

    // Build the shadow file map by re-applying each op.
    let shadow = build_shadow(&plan, &original);

    // Diff-based replacement count (for parity with preview's display).
    let total_replacements = shadow
        .iter()
        .filter(|(k, v)| original.get(*k).map(|o| o != *v).unwrap_or(false))
        .count();

    // Build the pipeline. SyntacticStage always runs; RuleStage runs only
    // when the workspace has rules loaded — rule packs gate applies via
    // the `violation(...)` convention documented in docs/rules-dsl.md.
    // CargoCheckStage is opt-in via `validation_profile = "typed"`.
    let stages: Vec<Box<dyn ValidationStage>> =
        build_pipeline(profile, &root, &rules).map_err(RpcError::invalid_params)?;
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
            let entry = crate::journal::new_entry(out.commit_id.clone(), plan_label, files);
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
    rules: &[pf_rules::Rule],
) -> Result<Vec<Box<dyn pf_validate::ValidationStage>>, String> {
    let mut stages: Vec<Box<dyn pf_validate::ValidationStage>> =
        vec![Box::new(pf_validate::SyntacticStage)];
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
            stages.push(Box::new(crate::validate_stages::CargoTestStage::new(
                workspace_root.to_path_buf(),
                std::time::Duration::from_secs(300),
            )));
        }
        other => {
            return Err(format!(
                "unknown validation_profile `{other}` (known: default, typed, tested)"
            ));
        }
    }
    Ok(stages)
}

fn build_shadow(
    plan: &pf_patch::PatchPlan,
    original: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let mut working = original.clone();
    for op in &plan.ops {
        match op {
            pf_patch::PatchOp::RenameFunction {
                old_name,
                new_name,
                files,
            } => {
                let paths: Vec<String> = if files.is_empty() {
                    working
                        .keys()
                        .filter(|p| p.ends_with(".rs"))
                        .cloned()
                        .collect()
                } else {
                    files
                        .iter()
                        .filter(|p| working.contains_key(p.as_str()))
                        .cloned()
                        .collect()
                };
                for path in paths {
                    if let Some(src) = working.get(&path).cloned() {
                        if let Ok((new_src, n)) =
                            pf_patch::rust_rename::rename(&src, old_name, new_name)
                        {
                            if n > 0 {
                                working.insert(path, new_src);
                            }
                        }
                    }
                }
            }
        }
    }
    working
}

fn to_validation_dto(r: &pf_validate::ValidationReport) -> ValidationReportDto {
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
                            pf_validate::Severity::Error => "error".into(),
                            pf_validate::Severity::Warning => "warning".into(),
                            pf_validate::Severity::Info => "info".into(),
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
            pf_llm::propose(
                core.llm_provider.as_ref(),
                &core.llm_cache,
                &mut st.graph,
                pf_llm::ProposeRequest {
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

    let prior_outcomes: Vec<pf_llm::ProposalOutcome> = p
        .prior_outcomes
        .into_iter()
        .map(|o| pf_llm::ProposalOutcome {
            predicate: o.predicate,
            args: o.args,
            justification: o.justification,
            accepted: o.accepted,
            rejection_reason: o.rejection_reason,
        })
        .collect();
    let prior_diagnostics: Vec<pf_llm::RefinerDiagnostic> = p
        .prior_diagnostics
        .into_iter()
        .map(|d| pf_llm::RefinerDiagnostic {
            severity: d.severity,
            file: d.file,
            message: d.message,
        })
        .collect();

    let result = core
        .with_workspace(&p.workspace_id, |_ws, st| {
            pf_llm::refine(
                core.llm_provider.as_ref(),
                &core.llm_cache,
                &mut st.graph,
                pf_llm::RefineRequest {
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

fn handle_explain_patch(core: &Core, params: Value) -> Result<Value, RpcError> {
    use pf_validate::{Pipeline, ValidationContext, ValidationStage};

    let p: ExplainPatchParams = decode(params)?;

    // Decode the plan the same way patch.preview/apply do.
    let mut ops: Vec<pf_patch::PatchOp> = Vec::with_capacity(p.plan.ops.len());
    for raw in &p.plan.ops {
        let op: pf_patch::PatchOp = serde_json::from_value(raw.clone())
            .map_err(|e| RpcError::invalid_params(format!("bad op: {e}")))?;
        ops.push(op);
    }
    let plan = pf_patch::PatchPlan::labelled(ops.clone(), p.plan.label.clone());
    let anchors: Vec<String> = anchors_from_ops(&ops);

    // Load originals + rules snapshot.
    let (root, original, rules) = core
        .with_workspace(&p.workspace_id, |w, st| {
            let root = std::path::PathBuf::from(&w.root);
            let mut map: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            for sf in pf_ingest::walk(&root, &pf_ingest::IngestOptions::default()) {
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
    let shadow = build_shadow(&plan, &original);
    let profile = p.validation_profile.as_deref().unwrap_or("default");
    let stages: Vec<Box<dyn ValidationStage>> =
        build_pipeline(profile, &root, &rules).map_err(RpcError::invalid_params)?;
    let validation = Pipeline::custom(stages).run(&ValidationContext {
        shadow_files: &shadow,
        original_files: &original,
    });

    // Adapt the validation report into the explainer's stage view.
    let stage_refs: Vec<pf_explain::ValidationStageRef> = validation
        .stages
        .iter()
        .map(|s| pf_explain::ValidationStageRef {
            name: s.stage.clone(),
            ok: s.ok,
            diagnostics: s
                .diagnostics
                .iter()
                .map(|d| pf_explain::model::StageDiagnostic {
                    severity: match d.severity {
                        pf_validate::Severity::Error => "error".into(),
                        pf_validate::Severity::Warning => "warning".into(),
                        pf_validate::Severity::Info => "info".into(),
                    },
                    file: d.file.clone(),
                    message: d.message.clone(),
                })
                .collect(),
        })
        .collect();

    // Candidate outcomes passed by the caller (may be empty).
    let candidate_refs: Vec<pf_explain::CandidateRef> = p
        .candidate_outcomes
        .iter()
        .map(|o| pf_explain::CandidateRef {
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
            pf_explain::build(pf_explain::ExplainInput {
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

fn anchors_from_ops(ops: &[pf_patch::PatchOp]) -> Vec<String> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            pf_patch::PatchOp::RenameFunction {
                old_name, new_name, ..
            } => {
                out.push(old_name.clone());
                out.push(new_name.clone());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn explanation_to_dto(e: pf_explain::Explanation) -> ExplainPatchResult {
    ExplainPatchResult {
        plan_label: e.plan_label,
        anchors: e.anchors,
        verdict: match e.verdict {
            pf_explain::Verdict::Accepted { commit_id, notes } => {
                VerdictDto::Accepted { commit_id, notes }
            }
            pf_explain::Verdict::Rejected {
                reason,
                failing_stages,
            } => VerdictDto::Rejected {
                reason,
                failing_stages,
            },
            pf_explain::Verdict::NotProven { reason } => VerdictDto::NotProven { reason },
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

fn evidence_to_dto(n: pf_explain::EvidenceNode) -> EvidenceNodeDto {
    match n {
        pf_explain::EvidenceNode::Observed {
            predicate,
            args,
            role,
        } => EvidenceNodeDto::Observed {
            predicate,
            args,
            role,
        },
        pf_explain::EvidenceNode::Inferred { predicate, args } => {
            EvidenceNodeDto::Inferred { predicate, args }
        }
        pf_explain::EvidenceNode::RuleActivation {
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
        pf_explain::EvidenceNode::Candidate(c) => EvidenceNodeDto::Candidate {
            predicate: c.predicate,
            args: c.args,
            justification: c.justification,
            accepted: c.accepted,
            rejection_reason: c.rejection_reason,
            round: c.round,
        },
        pf_explain::EvidenceNode::Stage {
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
            pf_rules::evaluate(&st.rules, &mut st.graph)
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
    use pf_protocol::{Id, Request};

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
