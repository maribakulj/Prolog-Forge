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
