//! Core-owned concrete `ValidationStage` implementations that need to cross
//! multiple crate boundaries (rule engine + analyzer + CSM lowering + the
//! filesystem) and therefore cannot live inside `pf-validate` without
//! pulling the dependency graph upside-down.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pf_graph::GraphStore;
use pf_lang_rust::RustAnalyzer;
use pf_rules::Rule;
use pf_validate::{Diagnostic, Severity, StageReport, ValidationContext, ValidationStage};

use pf_csm::LanguageAnalyzer;

/// Re-evaluates the workspace's rule set against the graph derived from the
/// **shadow** source files (not the on-disk ones). Any fact of predicate
/// `violation` derived from the shadow graph counts as a constraint
/// violation and fails the stage.
///
/// This is the Phase 1.5 convention: rule packs that want to gate applies
/// declare rules whose head is `violation(...)`. See `docs/rules-dsl.md`.
pub struct RuleStage {
    rules: Vec<Rule>,
}

impl RuleStage {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }
}

impl ValidationStage for RuleStage {
    fn name(&self) -> &'static str {
        "rules"
    }

    fn validate(&self, ctx: &ValidationContext<'_>) -> StageReport {
        if self.rules.is_empty() {
            return StageReport::ok(self.name());
        }

        // Build a fresh graph from the shadow sources.
        let analyzer = RustAnalyzer::new();
        let mut graph = GraphStore::new();
        let mut diags: Vec<Diagnostic> = Vec::new();
        for (path, src) in ctx.shadow_files {
            if !path.ends_with(".rs") {
                continue;
            }
            match analyzer.analyze(src, path) {
                Ok(frag) => {
                    for fact in crate::lower::lower(&frag) {
                        if let Err(e) = graph.insert(fact) {
                            diags.push(Diagnostic {
                                severity: Severity::Error,
                                file: Some(path.clone()),
                                message: format!("graph insert: {e}"),
                            });
                        }
                    }
                }
                Err(e) => {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        file: Some(path.clone()),
                        message: format!("shadow analyze: {}", e.message),
                    });
                }
            }
        }

        // Run the rule engine to fixpoint on the shadow graph.
        if let Err(e) = pf_rules::evaluate(&self.rules, &mut graph) {
            diags.push(Diagnostic {
                severity: Severity::Error,
                file: None,
                message: format!("rule engine: {e}"),
            });
            return StageReport::with_errors(self.name(), diags);
        }

        // Collect every derived violation fact.
        for fact in graph.facts_of("violation") {
            diags.push(Diagnostic {
                severity: Severity::Error,
                file: None,
                message: format!("violation({})", fact.args.join(", ")),
            });
        }

        StageReport::with_errors(self.name(), diags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_rules::parse as parse_rules;
    use std::collections::BTreeMap;

    #[test]
    fn no_rules_is_a_pass() {
        let shadow = BTreeMap::new();
        let original = BTreeMap::new();
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &original,
        };
        let stage = RuleStage::new(Vec::new());
        let r = stage.validate(&ctx);
        assert!(r.ok);
        assert_eq!(r.diagnostics.len(), 0);
    }

    #[test]
    fn violation_rule_fires_on_shadow() {
        // Rule: forbid any top-level function named `forbidden`.
        let src = r#"
            violation(F) :- function(F, forbidden).
        "#;
        let program = parse_rules(src).unwrap();
        let mut shadow = BTreeMap::new();
        shadow.insert("src/lib.rs".into(), "pub fn forbidden() {}".into());
        let original = shadow.clone();
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &original,
        };
        let stage = RuleStage::new(program.rules);
        let r = stage.validate(&ctx);
        assert!(!r.ok, "violation rule should fail the stage");
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.message.starts_with("violation(")));
    }

    #[test]
    fn no_violation_is_a_pass() {
        let src = r#"
            violation(F) :- function(F, forbidden).
        "#;
        let program = parse_rules(src).unwrap();
        let mut shadow = BTreeMap::new();
        shadow.insert("src/lib.rs".into(), "pub fn ok_fn() {}".into());
        let original = shadow.clone();
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &original,
        };
        let stage = RuleStage::new(program.rules);
        let r = stage.validate(&ctx);
        assert!(r.ok);
    }
}

/// Runs `cargo check --message-format=json` on a shadow copy of the workspace.
///
/// This is the first validation stage that grounds verdicts in the Rust
/// compiler rather than in hand-written Datalog: the stage materialises the
/// patched file set to a temp directory, shells out to cargo, and emits one
/// [`Diagnostic`] per compiler error. It is strictly opt-in (see
/// `validation_profile = "typed"` on `patch.apply` and `explain.patch`) —
/// `cargo check` is expensive enough that we never want it on the default
/// path.
///
/// When the workspace root has no `Cargo.toml`, the stage passes with an
/// info diagnostic (there's nothing to type-check). When `cargo` is not on
/// `PATH`, it reports a warning and passes — the stage is an oracle, not a
/// hard requirement.
pub struct CargoCheckStage {
    workspace_root: PathBuf,
    /// Wall-clock cap on the `cargo check` run. Applied by polling the
    /// child process; a timeout produces a single error diagnostic.
    timeout: Duration,
}

impl CargoCheckStage {
    pub fn new(workspace_root: PathBuf, timeout: Duration) -> Self {
        Self {
            workspace_root,
            timeout,
        }
    }
}

impl ValidationStage for CargoCheckStage {
    fn name(&self) -> &'static str {
        "cargo_check"
    }

    fn validate(&self, ctx: &ValidationContext<'_>) -> StageReport {
        let manifest = self.workspace_root.join("Cargo.toml");
        if !manifest.exists() {
            // No Cargo project at the root — nothing to type-check. This
            // is a legitimate pass (some workspaces are not Rust crates)
            // but we surface an info diagnostic so the explainer knows
            // the stage ran and did no work.
            return StageReport::with_errors(
                self.name(),
                vec![Diagnostic {
                    severity: Severity::Info,
                    file: None,
                    message: "no Cargo.toml at workspace root; cargo_check skipped".into(),
                }],
            );
        }

        // 1. Create a throwaway shadow workspace.
        let tmp = match tempfile::tempdir() {
            Ok(t) => t,
            Err(e) => return error_stage(self.name(), format!("tempdir: {e}")),
        };
        let shadow_root = tmp.path().join("project");
        if let Err(e) = mirror_dir(&self.workspace_root, &shadow_root) {
            return error_stage(self.name(), format!("mirror: {e}"));
        }

        // 2. Overlay the shadow file contents.
        for (rel, content) in ctx.shadow_files {
            let dest = shadow_root.join(rel);
            if let Some(parent) = dest.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return error_stage(self.name(), format!("mkdir {}: {e}", parent.display()));
                }
            }
            if let Err(e) = std::fs::write(&dest, content) {
                return error_stage(self.name(), format!("write {}: {e}", dest.display()));
            }
        }

        // 3. Spawn `cargo check --all-targets --message-format=json --quiet`.
        //    `--all-targets` is deliberate: without it, `cargo check`
        //    skips `#[cfg(test)]` modules, examples, and benches —
        //    meaning a patch that breaks a test body but not a lib
        //    body would slip through. The current rename is not
        //    scope-aware, so the common failure mode is an unresolved
        //    reference *inside a test*; we have to compile tests to
        //    see it.
        let mut cmd = std::process::Command::new("cargo");
        cmd.args(["check", "--all-targets", "--message-format=json", "--quiet"])
            .current_dir(&shadow_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                // No cargo on PATH — don't fail the whole apply over that;
                // the stage is an oracle, not a hard gate. Emit a warning
                // so the verdict honestly reflects the missing evidence.
                return StageReport::with_errors(
                    self.name(),
                    vec![Diagnostic {
                        severity: Severity::Warning,
                        file: None,
                        message: format!("cargo not available: {e}"),
                    }],
                );
            }
        };

        // 4. Wait with a timeout.
        let (status, stdout) = match wait_with_timeout(child, self.timeout) {
            Ok(x) => x,
            Err(msg) => return error_stage(self.name(), msg),
        };

        // 5. Parse JSON diagnostics from stdout. Each line is a JSON object;
        //    we keep the subset whose `reason == "compiler-message"` and
        //    whose nested `message.level` is `error`.
        let mut diags: Vec<Diagnostic> = Vec::new();
        for line in stdout.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
                continue;
            }
            let msg = match v.get("message") {
                Some(m) => m,
                None => continue,
            };
            let level = msg.get("level").and_then(|l| l.as_str()).unwrap_or("");
            let severity = match level {
                "error" | "error: internal compiler error" => Severity::Error,
                "warning" => Severity::Warning,
                _ => continue,
            };
            let text = msg
                .get("message")
                .and_then(|t| t.as_str())
                .unwrap_or("(no message)")
                .to_string();
            let file = msg
                .get("spans")
                .and_then(|s| s.as_array())
                .and_then(|a| a.first())
                .and_then(|s| s.get("file_name"))
                .and_then(|f| f.as_str())
                .map(|s| s.to_string());
            diags.push(Diagnostic {
                severity,
                file,
                message: text,
            });
        }

        // 6. If cargo exited non-zero but we produced no error diagnostic,
        //    surface a generic failure so the stage is honest. Happens when
        //    cargo fails for reasons outside its JSON stream (e.g. missing
        //    registry, offline with unlocked deps).
        if !status.success() && !diags.iter().any(|d| d.severity == Severity::Error) {
            diags.push(Diagnostic {
                severity: Severity::Error,
                file: None,
                message: format!(
                    "cargo check exited with status {} and no JSON error was emitted",
                    status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into())
                ),
            });
        }

        StageReport::with_errors(self.name(), diags)
    }
}

fn error_stage(name: &'static str, msg: String) -> StageReport {
    StageReport::with_errors(
        name,
        vec![Diagnostic {
            severity: Severity::Error,
            file: None,
            message: msg,
        }],
    )
}

/// Recursively copy `src` into `dst`, skipping `.prolog-forge`, `target`,
/// and hidden VCS directories. This is a cold-copy — small fixture projects
/// finish in milliseconds; bigger workspaces are why the stage is opt-in.
fn mirror_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in walkdir::WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != ".prolog-forge" && name != "target" && name != ".git"
        })
    {
        let entry = entry.map_err(|e| std::io::Error::other(e.to_string()))?;
        let rel = entry.path().strip_prefix(src).unwrap();
        let out = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&out)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &out)?;
        }
    }
    Ok(())
}

/// Wait for `child` to exit, killing it if it exceeds `timeout`. Returns
/// `(ExitStatus, stdout_utf8)`. Simple polling loop — avoids adding the
/// `wait-timeout` crate for a one-off usage.
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, String), String> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                if let Some(mut out) = child.stdout.take() {
                    use std::io::Read;
                    let _ = out.read_to_string(&mut stdout);
                }
                return Ok((status, stdout));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("cargo check timed out after {:?}", timeout));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
}

#[cfg(test)]
mod cargo_check_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn write_fixture(tmp: &std::path::Path, lib_src: &str) {
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(
            tmp.join("Cargo.toml"),
            "[package]\nname = \"pf_cargo_check_fixture\"\nversion = \"0.0.0\"\n\
             edition = \"2021\"\npublish = false\n[lib]\npath = \"src/lib.rs\"\n\
             [workspace]\n",
        )
        .unwrap();
        std::fs::write(tmp.join("src/lib.rs"), lib_src).unwrap();
    }

    #[test]
    fn skipped_when_no_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        // No Cargo.toml on purpose.
        let shadow: BTreeMap<String, String> = BTreeMap::new();
        let original = shadow.clone();
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &original,
        };
        let stage = CargoCheckStage::new(tmp.path().to_path_buf(), Duration::from_secs(120));
        let r = stage.validate(&ctx);
        assert!(r.ok);
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.message.contains("no Cargo.toml")));
    }

    #[test]
    fn passes_on_a_valid_project() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path(), "pub fn add(a: i32, b: i32) -> i32 { a + b }\n");
        let mut shadow: BTreeMap<String, String> = BTreeMap::new();
        shadow.insert(
            "src/lib.rs".into(),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n".into(),
        );
        let original = shadow.clone();
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &original,
        };
        let stage = CargoCheckStage::new(tmp.path().to_path_buf(), Duration::from_secs(180));
        let r = stage.validate(&ctx);
        assert!(r.ok, "cargo check should accept a valid shadow: {:?}", r);
    }

    #[test]
    fn fails_when_shadow_introduces_a_type_error() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path(), "pub fn add(a: i32, b: i32) -> i32 { a + b }\n");
        // Shadow introduces a type error: returning &str where i32 is expected.
        let mut shadow: BTreeMap<String, String> = BTreeMap::new();
        shadow.insert(
            "src/lib.rs".into(),
            "pub fn add(_a: i32, _b: i32) -> i32 { \"oops\" }\n".into(),
        );
        let original = shadow.clone();
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &original,
        };
        let stage = CargoCheckStage::new(tmp.path().to_path_buf(), Duration::from_secs(180));
        let r = stage.validate(&ctx);
        assert!(
            !r.ok,
            "cargo check must reject a shadow with a type error: {:?}",
            r
        );
        assert!(
            r.diagnostics.iter().any(|d| d.severity == Severity::Error),
            "expected at least one error-severity diagnostic: {:?}",
            r.diagnostics
        );
    }
}

/// Runs `cargo test --quiet --no-fail-fast` on a shadow copy of the workspace.
///
/// Where [`CargoCheckStage`] proves the patch type-checks, this stage proves
/// it preserves behavior: if any existing test fails under the proposed
/// sources, the apply is rejected. It is the behavioral half of the typed
/// validation story and uses the same mirror-and-overlay mechanics; the
/// only differences are the cargo subcommand and the stage's tolerance
/// envelope (tests can be slow).
///
/// Opt-in via `validation_profile = "tested"` on `patch.apply` /
/// `explain.patch`. When `cargo` is missing, or the workspace has no
/// `Cargo.toml`, the stage degrades gracefully (warning / info + pass).
pub struct CargoTestStage {
    workspace_root: PathBuf,
    /// Wall-clock cap on the `cargo test` run. Tests compile twice
    /// (once for the type-checker, once for the runner) the first time,
    /// so budgets need to be larger than for `CargoCheckStage`.
    timeout: Duration,
    /// Impacted test selection (Phase 1.16). `None` runs every test
    /// in the workspace — the safe, slow default. `Some(names)`
    /// invokes `cargo test name1 name2 …` so only tests whose full
    /// path contains any of the supplied substrings run. Empty
    /// `Some(vec![])` is treated the same as `None`; the stage never
    /// runs *zero* tests (better to re-run a suite than to silently
    /// skip coverage).
    test_selection: Option<Vec<String>>,
}

impl CargoTestStage {
    pub fn new(workspace_root: PathBuf, timeout: Duration) -> Self {
        Self {
            workspace_root,
            timeout,
            test_selection: None,
        }
    }

    /// Build a stage that restricts `cargo test` to the given
    /// substring-matched names. An empty vec is treated as "no
    /// selection" — the stage falls back to the full suite.
    pub fn with_selection(mut self, names: Vec<String>) -> Self {
        self.test_selection = if names.is_empty() { None } else { Some(names) };
        self
    }
}

impl ValidationStage for CargoTestStage {
    fn name(&self) -> &'static str {
        "cargo_test"
    }

    fn validate(&self, ctx: &ValidationContext<'_>) -> StageReport {
        let manifest = self.workspace_root.join("Cargo.toml");
        if !manifest.exists() {
            return StageReport::with_errors(
                self.name(),
                vec![Diagnostic {
                    severity: Severity::Info,
                    file: None,
                    message: "no Cargo.toml at workspace root; cargo_test skipped".into(),
                }],
            );
        }

        // Same shadow materialisation as CargoCheckStage. Factoring is
        // deliberate — we want the two stages independently debuggable.
        let tmp = match tempfile::tempdir() {
            Ok(t) => t,
            Err(e) => return error_stage(self.name(), format!("tempdir: {e}")),
        };
        let shadow_root = tmp.path().join("project");
        if let Err(e) = mirror_dir(&self.workspace_root, &shadow_root) {
            return error_stage(self.name(), format!("mirror: {e}"));
        }
        for (rel, content) in ctx.shadow_files {
            let dest = shadow_root.join(rel);
            if let Some(parent) = dest.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return error_stage(self.name(), format!("mkdir {}: {e}", parent.display()));
                }
            }
            if let Err(e) = std::fs::write(&dest, content) {
                return error_stage(self.name(), format!("write {}: {e}", dest.display()));
            }
        }

        // `--no-fail-fast` so we get every failing test, not just the
        // first. `--quiet` keeps the runner output compact. Pass a
        // deterministic test thread count so CI behavior is stable.
        // When `test_selection` is set we insert the substring filter
        // names *before* the `--` separator so cargo treats them as
        // filters on the set of test binaries / items it runs.
        let mut cmd = std::process::Command::new("cargo");
        cmd.arg("test").arg("--quiet").arg("--no-fail-fast");
        if let Some(names) = &self.test_selection {
            for n in names {
                cmd.arg(n);
            }
        }
        cmd.arg("--").arg("--test-threads=1");
        cmd.current_dir(&shadow_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return StageReport::with_errors(
                    self.name(),
                    vec![Diagnostic {
                        severity: Severity::Warning,
                        file: None,
                        message: format!("cargo not available: {e}"),
                    }],
                );
            }
        };

        let (status, combined) = match wait_with_output_and_timeout(child, self.timeout) {
            Ok(x) => x,
            Err(msg) => return error_stage(self.name(), msg),
        };

        if status.success() {
            return StageReport::ok(self.name());
        }

        // Non-zero exit: extract failing test names from the runner
        // output. The stable libtest format emits lines like:
        //     test tests::it_works ... FAILED
        // and a final summary:
        //     failures:
        //         tests::it_works
        // The exact wording is stable across cargo releases. We produce
        // one error Diagnostic per failing test name and a final
        // summary Diagnostic so the rejection reason is legible.
        let mut failures: Vec<String> = Vec::new();
        for line in combined.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("test ") {
                if let Some(name) = rest.strip_suffix(" ... FAILED") {
                    failures.push(name.trim().to_string());
                }
            }
        }
        failures.sort();
        failures.dedup();

        let mut diags: Vec<Diagnostic> = failures
            .into_iter()
            .map(|name| Diagnostic {
                severity: Severity::Error,
                file: None,
                message: format!("test `{name}` failed"),
            })
            .collect();
        if diags.is_empty() {
            // Exited non-zero but libtest did not announce a failure in
            // the recognised shape — surface the tail of the output so
            // the reviewer has something to go on.
            let tail: String = combined
                .lines()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .join("\n");
            diags.push(Diagnostic {
                severity: Severity::Error,
                file: None,
                message: format!(
                    "cargo test exited with status {} without a recognised failure line:\n{tail}",
                    status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into())
                ),
            });
        }
        StageReport::with_errors(self.name(), diags)
    }
}

/// Variant of [`wait_with_timeout`] that captures both stdout and stderr
/// interleaved. Tests print to stdout; runner metadata goes to stderr;
/// having both simplifies diagnostic extraction.
fn wait_with_output_and_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, String), String> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut s) = child.stdout.take() {
                    use std::io::Read;
                    let _ = s.read_to_string(&mut out);
                }
                if let Some(mut s) = child.stderr.take() {
                    use std::io::Read;
                    let _ = s.read_to_string(&mut out);
                }
                return Ok((status, out));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("cargo test timed out after {:?}", timeout));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
}

#[cfg(test)]
mod cargo_test_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn write_fixture(tmp: &std::path::Path, lib_src: &str) {
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(
            tmp.join("Cargo.toml"),
            "[package]\nname = \"pf_cargo_test_fixture\"\nversion = \"0.0.0\"\n\
             edition = \"2021\"\npublish = false\n[lib]\npath = \"src/lib.rs\"\n\
             [workspace]\n",
        )
        .unwrap();
        std::fs::write(tmp.join("src/lib.rs"), lib_src).unwrap();
    }

    const LIB_PLUS_TEST: &str = "\
pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn add_is_correct() {
        assert_eq!(add(1, 2), 3);
    }
}
";

    #[test]
    fn passes_when_shadow_preserves_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path(), LIB_PLUS_TEST);
        // Shadow is identical to the on-disk file. Tests must pass.
        let mut shadow: BTreeMap<String, String> = BTreeMap::new();
        shadow.insert("src/lib.rs".into(), LIB_PLUS_TEST.into());
        let original = shadow.clone();
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &original,
        };
        let stage = CargoTestStage::new(tmp.path().to_path_buf(), Duration::from_secs(180));
        let r = stage.validate(&ctx);
        assert!(r.ok, "expected behavioral pass, got {:?}", r);
    }

    #[test]
    fn fails_when_shadow_breaks_a_test() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path(), LIB_PLUS_TEST);
        // Broken shadow: add now subtracts, so `add(1, 2) == 3` fails.
        let broken = LIB_PLUS_TEST.replace("a + b", "a - b");
        let mut shadow: BTreeMap<String, String> = BTreeMap::new();
        shadow.insert("src/lib.rs".into(), broken);
        let original: BTreeMap<String, String> = BTreeMap::new();
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &original,
        };
        let stage = CargoTestStage::new(tmp.path().to_path_buf(), Duration::from_secs(180));
        let r = stage.validate(&ctx);
        assert!(!r.ok, "expected behavioral rejection: {:?}", r);
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.message.contains("add_is_correct") || d.message.contains("failed")),
            "expected a diagnostic naming the failing test or failure: {:?}",
            r.diagnostics
        );
    }

    /// Phase 1.16: passing a narrowed `test_selection` must cause
    /// `cargo test` to actually skip non-matching tests. The fixture
    /// has one passing test and one *broken* test — selecting only
    /// the passing one must turn the stage green, while running both
    /// (empty selection) reports the failure.
    #[test]
    fn impacted_selection_skips_unrelated_tests() {
        const TWO_TESTS: &str = r#"
pub fn add(a: i32, b: i32) -> i32 { a + b }
pub fn sub(a: i32, b: i32) -> i32 { a - b }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adds() { assert_eq!(add(1, 2), 3); }
    #[test]
    fn subs_broken() { assert_eq!(sub(5, 3), 99); }  // deliberately wrong
}
"#;
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path(), TWO_TESTS);
        let mut shadow: BTreeMap<String, String> = BTreeMap::new();
        shadow.insert("src/lib.rs".into(), TWO_TESTS.into());
        let ctx = ValidationContext {
            shadow_files: &shadow,
            original_files: &shadow,
        };
        // Full suite → the broken test fails the stage.
        let all =
            CargoTestStage::new(tmp.path().to_path_buf(), Duration::from_secs(180)).validate(&ctx);
        assert!(!all.ok, "full suite must see the broken test: {:?}", all);
        // Narrow to `adds` only → passes.
        let narrowed = CargoTestStage::new(tmp.path().to_path_buf(), Duration::from_secs(180))
            .with_selection(vec!["adds".into()])
            .validate(&ctx);
        assert!(
            narrowed.ok,
            "selection must skip the unrelated broken test: {:?}",
            narrowed
        );
    }
}
