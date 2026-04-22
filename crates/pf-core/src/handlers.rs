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
        METHOD_PATCH_PREVIEW => handle_patch_preview(core, params),
        METHOD_PATCH_APPLY => handle_patch_apply(core, params),
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
            METHOD_PATCH_PREVIEW.into(),
            METHOD_PATCH_APPLY.into(),
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
    use pf_validate::{Pipeline, ValidationContext};

    let p: PatchApplyParams = decode(params)?;

    // Decode plan (shared shape with preview).
    let mut ops: Vec<pf_patch::PatchOp> = Vec::with_capacity(p.plan.ops.len());
    for raw in p.plan.ops {
        let op: pf_patch::PatchOp = serde_json::from_value(raw)
            .map_err(|e| RpcError::invalid_params(format!("bad op: {e}")))?;
        ops.push(op);
    }
    let plan = pf_patch::PatchPlan::labelled(ops, p.plan.label);

    // Load originals + root.
    let (root, original) = core
        .with_workspace(&p.workspace_id, |w, _st| {
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
            (root, map)
        })
        .map_err(RpcError::invalid_params)?;

    // Build the shadow file map by re-applying each op.
    let shadow = build_shadow(&plan, &original);

    // Diff-based replacement count (for parity with preview's display).
    let total_replacements = shadow
        .iter()
        .filter(|(k, v)| original.get(*k).map(|o| o != *v).unwrap_or(false))
        .count();

    // Run validation pipeline.
    let validation = Pipeline::syntactic_only().run(&ValidationContext {
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
        Ok(out) => Ok(serde_json::to_value(PatchApplyResult {
            applied: true,
            commit_id: Some(out.commit_id),
            files_written: out.files_written,
            bytes_written: out.bytes_written,
            total_replacements,
            validation: validation_dto,
            rejection_reason: None,
        })
        .unwrap()),
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
            })
            .collect(),
    })
    .unwrap())
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
