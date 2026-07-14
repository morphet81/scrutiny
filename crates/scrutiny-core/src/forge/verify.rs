//! Deterministic post-implementation verify gate.
//!
//! Runs the project's tests / lint / coverage, parses failures into a *minimal*
//! signal (failing test names + `file:line`, uncovered line ranges), and hands
//! only that to a fresh fix agent. The host — not the AI — decides green/red.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// One command to run as part of the gate. `framework` selects the failure parser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyCmd {
    pub command: String,
    pub framework: Option<String>,
}

/// How to measure total coverage % (and where per-file gaps live).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageProbe {
    pub command: String,
    pub summary_file: String,
    pub framework: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyPlan {
    pub commands: Vec<VerifyCmd>,
    pub coverage: Option<CoverageProbe>,
    pub coverage_target: u32,
    pub max_loops: u32,
}

impl VerifyPlan {
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// A single failing test, reduced to what a fix agent needs to act.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestFailure {
    pub name: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub message: String,
}

/// Uncovered lines for one file, ranges compressed (e.g. `12-15,40,88-90`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileGaps {
    pub file: String,
    pub lines: String,
}

/// Surgical failure signal fed to the fix agent — never raw logs unless parsing failed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FailureReport {
    pub failed_tests: Vec<TestFailure>,
    pub uncovered: Vec<FileGaps>,
    pub raw_tail: Option<String>,
}

impl FailureReport {
    pub fn is_clean(&self) -> bool {
        self.failed_tests.is_empty() && self.uncovered.is_empty() && self.raw_tail.is_none()
    }
}

const MAX_TESTS: usize = 15;
const MAX_MSG: usize = 200;
const MAX_TAIL: usize = 400;
const MAX_COV_FILES: usize = 20;

use crate::forge::context::TestHarnessHints;

/// Build the gate plan. Config commands win verbatim; otherwise derive tests,
/// e2e, lint/build and coverage from the sniffed harness + project files.
pub fn build_verify_plan(
    cwd: &Path,
    cfg_commands: &[String],
    harness: &TestHarnessHints,
    e2e: bool,
    coverage: bool,
    coverage_target: u32,
    max_loops: u32,
) -> VerifyPlan {
    if !cfg_commands.is_empty() {
        return VerifyPlan {
            commands: cfg_commands
                .iter()
                .filter(|c| !c.trim().is_empty())
                .map(|c| VerifyCmd {
                    command: c.clone(),
                    framework: None,
                })
                .collect(),
            coverage: None,
            coverage_target,
            max_loops,
        };
    }

    let mut commands = Vec::new();
    if let Some(unit) = harness.unit_framework.as_deref() {
        if let Some(cmd) = unit_command(unit) {
            commands.push(VerifyCmd {
                command: cmd.into(),
                framework: Some(unit.into()),
            });
        }
    }
    if e2e {
        if let Some(e2e_fw) = harness.e2e_framework.as_deref() {
            if let Some(cmd) = e2e_command(e2e_fw) {
                commands.push(VerifyCmd {
                    command: cmd.into(),
                    framework: Some(e2e_fw.into()),
                });
            }
        }
    }

    commands.extend(derive_lint_build(cwd));

    let cov_probe = if coverage {
        harness
            .unit_framework
            .as_deref()
            .and_then(coverage_probe)
    } else {
        None
    };

    VerifyPlan {
        commands,
        coverage: cov_probe,
        coverage_target,
        max_loops,
    }
}

fn unit_command(framework: &str) -> Option<&'static str> {
    match framework {
        "vitest" => Some("npx vitest run --reporter=json"),
        "jest" => Some("npx jest --json"),
        "pytest" => Some("pytest -q --tb=line"),
        "cargo-test" => Some("cargo test"),
        "phpunit" => Some("vendor/bin/phpunit"),
        _ => None,
    }
}

fn e2e_command(framework: &str) -> Option<&'static str> {
    match framework {
        "playwright" => Some("npx playwright test --reporter=json"),
        "cypress" => Some("npx cypress run"),
        _ => None,
    }
}

/// Lint / typecheck / build commands derived from project files. `framework:
/// None` → failures land in `raw_tail` (lint tools already print `file:line`).
fn derive_lint_build(cwd: &Path) -> Vec<VerifyCmd> {
    let mut cmds = Vec::new();
    let vc = |c: &str| VerifyCmd {
        command: c.to_string(),
        framework: None,
    };

    if cwd.join("Cargo.toml").exists() {
        cmds.push(vc("cargo clippy --all-targets"));
    }

    match read_npm_scripts(cwd) {
        Some(scripts) => {
            let pm = detect_pm(cwd);
            for name in ["typecheck", "lint", "build"] {
                if scripts.iter().any(|s| s == name) {
                    cmds.push(vc(&format!("{pm} run {name}")));
                }
            }
            if !scripts.iter().any(|s| s == "typecheck") && cwd.join("tsconfig.json").exists() {
                cmds.push(vc("npx tsc --noEmit"));
            }
        }
        None => {
            if cwd.join("tsconfig.json").exists() {
                cmds.push(vc("npx tsc --noEmit"));
            }
        }
    }

    cmds
}

fn read_npm_scripts(cwd: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(cwd.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let scripts = v.get("scripts")?.as_object()?;
    Some(scripts.keys().cloned().collect())
}

fn detect_pm(cwd: &Path) -> &'static str {
    if cwd.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if cwd.join("yarn.lock").exists() {
        "yarn"
    } else {
        "npm"
    }
}

fn coverage_probe(framework: &str) -> Option<CoverageProbe> {
    let (command, summary_file) = match framework {
        "vitest" => (
            "npx vitest run --coverage --coverage.reporter=json-summary --coverage.reporter=json",
            "coverage/coverage-summary.json",
        ),
        "jest" => (
            "npx jest --coverage --coverageReporters=json-summary --coverageReporters=json",
            "coverage/coverage-summary.json",
        ),
        "pytest" => ("pytest --cov --cov-report=json", "coverage.json"),
        "cargo-test" => (
            "cargo llvm-cov --json --output-path .scrutiny/cov.json",
            ".scrutiny/cov.json",
        ),
        // phpunit coverage needs xdebug/clover — skip.
        _ => return None,
    };
    Some(CoverageProbe {
        command: command.into(),
        summary_file: summary_file.into(),
        framework: framework.into(),
    })
}

/// Run a shell command in `cwd`, capturing exit code + stdout + stderr.
pub fn run_command(cwd: &Path, cmd: &str) -> (i32, String, String) {
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let flag = if cfg!(windows) { "/C" } else { "-c" };
    match Command::new(shell)
        .args([flag, cmd])
        .current_dir(cwd)
        .output()
    {
        Ok(out) => (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ),
        Err(e) => (-1, String::new(), format!("failed to spawn `{cmd}`: {e}")),
    }
}

/// Parse test-runner output into a minimal list of failures. Empty vec + a
/// `raw_tail` fallback (via the caller) when the framework is unknown or JSON
/// parsing fails.
pub fn parse_test_failures(framework: Option<&str>, stdout: &str, stderr: &str) -> Vec<TestFailure> {
    match framework {
        Some("vitest") | Some("jest") => parse_jest_json(stdout),
        Some("playwright") => parse_playwright_json(stdout),
        Some("pytest") => parse_pytest(stdout, stderr),
        Some("cargo-test") => parse_cargo(stdout, stderr),
        _ => Vec::new(),
    }
}

fn strip_ansi(s: &str) -> String {
    // Drop CSI escape sequences: ESC [ ... letter.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// Pull the first `path:line[:col]` frame out of a message.
fn file_line_from_msg(msg: &str) -> (Option<String>, Option<u32>) {
    for tok in msg.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
        let tok = tok.trim_matches(|c| c == '"' || c == '\'');
        // Want <path>:<line> where path contains a '/' or '.' and line is digits.
        let mut parts = tok.rsplitn(3, ':');
        let maybe_line = parts.next();
        let maybe_mid = parts.next();
        let maybe_path = parts.next();
        // token was file:line
        if let (Some(path), Some(line)) = (maybe_mid, maybe_line) {
            if maybe_path.is_none() {
                if let Ok(n) = line.parse::<u32>() {
                    if (path.contains('/') || path.contains('.')) && !path.is_empty() {
                        return (Some(path.to_string()), Some(n));
                    }
                }
            } else if let Ok(n) = maybe_mid.unwrap_or("").parse::<u32>() {
                // token was file:line:col
                if let Some(path) = maybe_path {
                    if (path.contains('/') || path.contains('.')) && !path.is_empty() {
                        return (Some(path.to_string()), Some(n));
                    }
                }
                let _ = n;
            }
        }
    }
    (None, None)
}

fn parse_jest_json(stdout: &str) -> Vec<TestFailure> {
    let v: serde_json::Value = match find_json(stdout) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let results = v.get("testResults").and_then(|x| x.as_array());
    if let Some(files) = results {
        for f in files {
            let file = f
                .get("name")
                .or_else(|| f.get("testFilePath"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            if let Some(asserts) = f.get("assertionResults").and_then(|x| x.as_array()) {
                for a in asserts {
                    if a.get("status").and_then(|x| x.as_str()) != Some("failed") {
                        continue;
                    }
                    let name = a
                        .get("fullName")
                        .or_else(|| a.get("title"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("<unnamed test>")
                        .to_string();
                    let raw_msg = a
                        .get("failureMessages")
                        .and_then(|x| x.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    let msg = strip_ansi(raw_msg);
                    let first_line = msg.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                    let (fl_file, fl_line) = file_line_from_msg(&msg);
                    out.push(TestFailure {
                        name,
                        file: fl_file.or_else(|| file.clone()),
                        line: fl_line,
                        message: clip(first_line, MAX_MSG),
                    });
                    if out.len() >= MAX_TESTS {
                        return out;
                    }
                }
            }
        }
    }
    out
}

fn parse_playwright_json(stdout: &str) -> Vec<TestFailure> {
    let v: serde_json::Value = match find_json(stdout) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    fn walk(node: &serde_json::Value, out: &mut Vec<TestFailure>) {
        if out.len() >= MAX_TESTS {
            return;
        }
        if let Some(specs) = node.get("specs").and_then(|x| x.as_array()) {
            for spec in specs {
                let ok = spec.get("ok").and_then(|x| x.as_bool()).unwrap_or(true);
                if ok {
                    continue;
                }
                let title = spec.get("title").and_then(|x| x.as_str()).unwrap_or("<spec>");
                let file = spec
                    .get("file")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string());
                let line = spec.get("line").and_then(|x| x.as_u64()).map(|n| n as u32);
                out.push(TestFailure {
                    name: title.to_string(),
                    file,
                    line,
                    message: "spec failed".into(),
                });
                if out.len() >= MAX_TESTS {
                    return;
                }
            }
        }
        if let Some(suites) = node.get("suites").and_then(|x| x.as_array()) {
            for s in suites {
                walk(s, out);
            }
        }
    }
    walk(&v, &mut out);
    out
}

fn parse_pytest(stdout: &str, stderr: &str) -> Vec<TestFailure> {
    let mut out = Vec::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim();
        // `--tb=line` emits `path:line: ExcType: message`
        if !line.contains(": ") {
            continue;
        }
        let mut it = line.splitn(3, ':');
        let path = it.next().unwrap_or("").trim();
        let lineno = it.next().unwrap_or("").trim();
        let rest = it.next().unwrap_or("").trim();
        if lineno.parse::<u32>().is_ok()
            && (path.ends_with(".py"))
            && !rest.is_empty()
        {
            out.push(TestFailure {
                name: format!("{path}:{lineno}"),
                file: Some(path.to_string()),
                line: lineno.parse().ok(),
                message: clip(rest, MAX_MSG),
            });
            if out.len() >= MAX_TESTS {
                break;
            }
        }
    }
    out
}

fn parse_cargo(stdout: &str, stderr: &str) -> Vec<TestFailure> {
    let combined = format!("{stdout}\n{stderr}");
    // Names: lines in the trailing "failures:" list are `    <name>`.
    // Panics: `thread '<name>' panicked at <file>:<line>:<col>:`.
    let mut panics: BTreeMap<String, (Option<String>, Option<u32>, String)> = BTreeMap::new();
    let lines: Vec<&str> = combined.lines().collect();
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("thread '") {
            if let Some(end) = rest.find('\'') {
                let name = &rest[..end];
                let after = &rest[end..];
                if let Some(at) = after.find("panicked at ") {
                    let loc = after[at + "panicked at ".len()..].trim_end_matches(':').trim();
                    let (f, l) = split_file_line(loc);
                    let msg = lines
                        .get(i + 1)
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("panicked")
                        .to_string();
                    panics.insert(name.to_string(), (f, l, clip(&msg, MAX_MSG)));
                }
            }
        }
    }

    let mut out = Vec::new();
    let mut in_failures = false;
    for raw in &lines {
        let line = raw.trim_end();
        if line.trim() == "failures:" {
            in_failures = true;
            continue;
        }
        if in_failures {
            let t = line.trim();
            if t.is_empty() || t.starts_with("test result") {
                in_failures = false;
                continue;
            }
            // Skip the per-test stdout dumps section header line variant.
            if t.contains("----") {
                continue;
            }
            let name = t.to_string();
            let (file, line_no, message) = panics
                .get(&name)
                .cloned()
                .unwrap_or((None, None, "test failed".into()));
            out.push(TestFailure {
                name,
                file,
                line: line_no,
                message,
            });
            if out.len() >= MAX_TESTS {
                break;
            }
        }
    }
    // Fallback: no explicit list but we saw panics.
    if out.is_empty() {
        for (name, (file, line, message)) in panics.into_iter().take(MAX_TESTS) {
            out.push(TestFailure {
                name,
                file,
                line,
                message,
            });
        }
    }
    out
}

fn split_file_line(loc: &str) -> (Option<String>, Option<u32>) {
    let mut parts = loc.splitn(3, ':');
    let file = parts.next().map(|s| s.to_string());
    let line = parts.next().and_then(|s| s.parse::<u32>().ok());
    (file.filter(|f| !f.is_empty()), line)
}

/// Locate the first parseable JSON object in noisy stdout (tools print logs first).
fn find_json(s: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s.trim()) {
        return Some(v);
    }
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        match b {
            b'"' if !esc => in_str = !in_str,
            b'\\' if in_str => {
                esc = !esc;
                continue;
            }
            b'{' if !in_str => depth += 1,
            b'}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    if let Ok(v) = serde_json::from_str(&s[start..=i]) {
                        return Some(v);
                    }
                    return None;
                }
            }
            _ => {}
        }
        esc = false;
    }
    None
}

/// Run the coverage probe once and return the total line-coverage %.
/// `None` on any tool/file/parse failure (unmeasurable — caller warns, never blocks).
pub fn measure_coverage(cwd: &Path, probe: &CoverageProbe) -> Option<f64> {
    let (_code, _out, _err) = run_command(cwd, &probe.command);
    let path = cwd.join(&probe.summary_file);
    let text = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    match probe.framework.as_str() {
        "vitest" | "jest" => v
            .pointer("/total/lines/pct")
            .and_then(|x| x.as_f64()),
        "pytest" => v
            .pointer("/totals/percent_covered")
            .and_then(|x| x.as_f64()),
        "cargo-test" => v
            .pointer("/data/0/totals/lines/percent")
            .and_then(|x| x.as_f64()),
        _ => None,
    }
}

/// Per-file uncovered line ranges from the coverage artifact.
pub fn coverage_gaps(framework: &str, cwd: &Path) -> Vec<FileGaps> {
    match framework {
        "vitest" | "jest" => istanbul_gaps(cwd),
        "pytest" => pytest_gaps(cwd),
        _ => Vec::new(),
    }
}

fn compress_ranges(mut lines: Vec<u32>) -> String {
    lines.sort_unstable();
    lines.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = lines[i];
        let mut end = start;
        while i + 1 < lines.len() && lines[i + 1] == end + 1 {
            end = lines[i + 1];
            i += 1;
        }
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}-{end}"));
        }
        i += 1;
    }
    parts.join(",")
}

fn under_repo(file: &str) -> bool {
    !file.contains("node_modules") && !file.contains("/vendor/") && !file.contains("/target/")
}

fn istanbul_gaps(cwd: &Path) -> Vec<FileGaps> {
    let text = match std::fs::read_to_string(cwd.join("coverage/coverage-final.json")) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let mut gaps = Vec::new();
    for (file, data) in obj {
        if !under_repo(file) {
            continue;
        }
        let stmt_map = data.get("statementMap").and_then(|x| x.as_object());
        let counts = data.get("s").and_then(|x| x.as_object());
        let (stmt_map, counts) = match (stmt_map, counts) {
            (Some(a), Some(b)) => (a, b),
            _ => continue,
        };
        let mut uncovered = Vec::new();
        for (id, hits) in counts {
            if hits.as_u64().unwrap_or(1) != 0 {
                continue;
            }
            if let Some(line) = stmt_map
                .get(id)
                .and_then(|s| s.pointer("/start/line"))
                .and_then(|x| x.as_u64())
            {
                uncovered.push(line as u32);
            }
        }
        if !uncovered.is_empty() {
            gaps.push(FileGaps {
                file: file.clone(),
                lines: compress_ranges(uncovered),
            });
        }
        if gaps.len() >= MAX_COV_FILES {
            break;
        }
    }
    gaps
}

fn pytest_gaps(cwd: &Path) -> Vec<FileGaps> {
    let text = match std::fs::read_to_string(cwd.join("coverage.json")) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let files = match v.get("files").and_then(|x| x.as_object()) {
        Some(f) => f,
        None => return Vec::new(),
    };
    let mut gaps = Vec::new();
    for (file, data) in files {
        if !under_repo(file) {
            continue;
        }
        if let Some(missing) = data.get("missing_lines").and_then(|x| x.as_array()) {
            let lines: Vec<u32> = missing
                .iter()
                .filter_map(|x| x.as_u64())
                .map(|n| n as u32)
                .collect();
            if !lines.is_empty() {
                gaps.push(FileGaps {
                    file: file.clone(),
                    lines: compress_ranges(lines),
                });
            }
        }
        if gaps.len() >= MAX_COV_FILES {
            break;
        }
    }
    gaps
}

/// Truncated stderr-else-stdout tail for the fallback case (parse failed).
pub fn raw_tail(stdout: &str, stderr: &str) -> String {
    let src = if !stderr.trim().is_empty() { stderr } else { stdout };
    let stripped = strip_ansi(src);
    let trimmed = stripped.trim();
    let count = trimmed.chars().count();
    if count <= MAX_TAIL {
        return trimmed.to_string();
    }
    trimmed.chars().skip(count - MAX_TAIL).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness(unit: Option<&str>, e2e: Option<&str>) -> TestHarnessHints {
        TestHarnessHints {
            unit_framework: unit.map(|s| s.to_string()),
            e2e_framework: e2e.map(|s| s.to_string()),
            test_dirs: vec![],
            config_files: vec![],
        }
    }

    // Empty dir → no lint/build derivation, keeps command-count assertions exact.
    fn empty_dir() -> std::path::PathBuf {
        Path::new("/nonexistent-scrutiny-verify-test").to_path_buf()
    }

    #[test]
    fn config_commands_win_and_disable_coverage() {
        let h = harness(Some("vitest"), None);
        let plan = build_verify_plan(
            &empty_dir(),
            &["make test".into(), "make lint".into()],
            &h,
            false,
            true,
            90,
            2,
        );
        assert_eq!(plan.commands.len(), 2);
        assert_eq!(plan.commands[0].command, "make test");
        assert!(plan.commands[0].framework.is_none());
        assert!(plan.coverage.is_none());
    }

    #[test]
    fn derives_unit_and_gates_e2e() {
        let d = empty_dir();
        let h = harness(Some("vitest"), Some("playwright"));
        let no_e2e = build_verify_plan(&d, &[], &h, false, false, 100, 2);
        assert_eq!(no_e2e.commands.len(), 1);
        assert_eq!(no_e2e.commands[0].command, "npx vitest run --reporter=json");

        let with_e2e = build_verify_plan(&d, &[], &h, true, false, 100, 2);
        assert_eq!(with_e2e.commands.len(), 2);
        assert_eq!(
            with_e2e.commands[1].command,
            "npx playwright test --reporter=json"
        );
    }

    #[test]
    fn coverage_probe_only_when_enabled() {
        let d = empty_dir();
        let h = harness(Some("pytest"), None);
        assert!(build_verify_plan(&d, &[], &h, false, false, 100, 2)
            .coverage
            .is_none());
        let p = build_verify_plan(&d, &[], &h, false, true, 80, 2).coverage;
        assert!(p.is_some());
        assert_eq!(p.unwrap().summary_file, "coverage.json");
    }

    #[test]
    fn unknown_framework_no_commands() {
        let h = harness(Some("mocha"), None);
        assert!(build_verify_plan(&empty_dir(), &[], &h, false, true, 100, 2).is_empty());
    }

    #[test]
    fn derives_lint_build_from_project_files() {
        let dir = std::env::temp_dir().join(format!("verify-lb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"lint":"eslint .","build":"vite build","test":"vitest"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();

        let h = harness(None, None);
        let cmds: Vec<String> = build_verify_plan(&dir, &[], &h, false, false, 100, 2)
            .commands
            .into_iter()
            .map(|c| c.command)
            .collect();
        std::fs::remove_dir_all(&dir).ok();

        assert!(cmds.iter().any(|c| c == "cargo clippy --all-targets"));
        assert!(cmds.iter().any(|c| c == "pnpm run lint"));
        assert!(cmds.iter().any(|c| c == "pnpm run build"));
        // no typecheck script but tsconfig present → tsc fallback
        assert!(cmds.iter().any(|c| c == "npx tsc --noEmit"));
    }

    #[test]
    fn jest_json_failures() {
        let json = r#"{"testResults":[{"name":"/repo/src/a.test.ts","assertionResults":[
          {"status":"passed","fullName":"adds"},
          {"status":"failed","fullName":"subtracts negatives","failureMessages":["Error: expected 1 got 2\n    at /repo/src/a.test.ts:42:7"]}
        ]}]}"#;
        let f = parse_test_failures(Some("jest"), json, "");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].name, "subtracts negatives");
        assert_eq!(f[0].file.as_deref(), Some("/repo/src/a.test.ts"));
        assert_eq!(f[0].line, Some(42));
        assert!(f[0].message.contains("expected 1 got 2"));
    }

    #[test]
    fn jest_json_with_log_prefix() {
        let noisy = "some log line\n{\"testResults\":[{\"name\":\"x\",\"assertionResults\":[{\"status\":\"failed\",\"fullName\":\"t\",\"failureMessages\":[\"boom\"]}]}]}\n";
        let f = parse_test_failures(Some("vitest"), noisy, "");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].name, "t");
    }

    #[test]
    fn pytest_tb_line() {
        let out = "tests/test_x.py:12: AssertionError: assert 1 == 2\nsome noise\ntests/test_y.py:3: ValueError: bad\n";
        let f = parse_test_failures(Some("pytest"), out, "");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].file.as_deref(), Some("tests/test_x.py"));
        assert_eq!(f[0].line, Some(12));
    }

    #[test]
    fn cargo_failures_list_and_panic() {
        let out = "\
running 2 tests
test math::adds ... ok
test math::subs ... FAILED

failures:

---- math::subs stdout ----
thread 'math::subs' panicked at src/math.rs:20:5:
assertion `left == right` failed

failures:
    math::subs

test result: FAILED. 1 passed; 1 failed";
        let f = parse_test_failures(Some("cargo-test"), out, "");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].name, "math::subs");
        assert_eq!(f[0].file.as_deref(), Some("src/math.rs"));
        assert_eq!(f[0].line, Some(20));
    }

    #[test]
    fn unknown_framework_yields_no_structured() {
        assert!(parse_test_failures(None, "boom", "").is_empty());
    }

    #[test]
    fn range_compression() {
        assert_eq!(compress_ranges(vec![12, 13, 14, 15, 40, 88, 89, 90]), "12-15,40,88-90");
        assert_eq!(compress_ranges(vec![5, 5, 3]), "3,5");
    }

    #[test]
    fn istanbul_gaps_from_fixture() {
        let dir = std::env::temp_dir().join(format!("verify-cov-{}", std::process::id()));
        let cov = dir.join("coverage");
        std::fs::create_dir_all(&cov).unwrap();
        let json = r#"{
          "/repo/src/a.ts": {"statementMap":{"0":{"start":{"line":3}},"1":{"start":{"line":4}},"2":{"start":{"line":5}}},"s":{"0":1,"1":0,"2":0}},
          "/repo/node_modules/x.js": {"statementMap":{"0":{"start":{"line":1}}},"s":{"0":0}}
        }"#;
        std::fs::write(cov.join("coverage-final.json"), json).unwrap();
        let gaps = coverage_gaps("vitest", &dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].file, "/repo/src/a.ts");
        assert_eq!(gaps[0].lines, "4-5");
    }

    #[test]
    fn raw_tail_truncates_tail() {
        let long = "x".repeat(1000);
        let t = raw_tail(&long, "");
        assert_eq!(t.chars().count(), MAX_TAIL);
    }
}
