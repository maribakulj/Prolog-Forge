//! Pipeline that runs stages in order and aggregates their reports.

use serde::{Deserialize, Serialize};

use crate::stage::{StageReport, ValidationContext, ValidationStage};
use crate::stages::SyntacticStage;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidationReport {
    pub ok: bool,
    pub stages: Vec<StageReport>,
}

pub struct Pipeline {
    stages: Vec<Box<dyn ValidationStage>>,
    /// If true, stop after the first failing stage. Default: true. Later
    /// stages that are expensive (tests, oracle) benefit from short-circuit.
    pub fail_fast: bool,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::syntactic_only()
    }
}

impl Pipeline {
    pub fn syntactic_only() -> Self {
        Self {
            stages: vec![Box::new(SyntacticStage)],
            fail_fast: true,
        }
    }

    pub fn custom(stages: Vec<Box<dyn ValidationStage>>) -> Self {
        Self {
            stages,
            fail_fast: true,
        }
    }

    pub fn run(&self, ctx: &ValidationContext<'_>) -> ValidationReport {
        let mut report = ValidationReport {
            ok: true,
            stages: Vec::new(),
        };
        for stage in &self.stages {
            let r = stage.validate(ctx);
            let ok = r.ok;
            report.stages.push(r);
            if !ok {
                report.ok = false;
                if self.fail_fast {
                    break;
                }
            }
        }
        report
    }
}
