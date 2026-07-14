//! Headless agent CLI runner + finding collation for `scrutiny review`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::paths::{temp_artifact_path, write_json_pretty};
use crate::plan::ConfirmedPlan;
use crate::review_session::{partition_pack_paths, ReviewAgentRecord};
use crate::runtime::DetectedClient;
use crate::scan::normalize_severity;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessKind {
    /// Read-focused specialist (no team spawn).
    Isolated,
    /// Lead agent may spawn a team.
    TeamLead,
    /// Follow-up Q&A.
    Ask,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentFinding {
    pub path: String,
    pub line: u32,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub severity: String,
    pub title: String,
    #[serde(default)]
    pub explanation: String,
    #[serde(default)]
    pub proposed_fix: String,
    #[serde(default)]
    pub fix_options: Vec<String>,
    #[serde(default)]
    pub source_role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRunResult {
    pub role: String,
    pub index: u32,
    pub paths: Vec<String>,
    pub findings: Vec<AgentFinding>,
    pub ok: bool,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewReport {
    pub version: u32,
    pub spawn_mode: String,
    pub model: String,
    pub findings: Vec<AgentFinding>,
    pub agents: Vec<AgentRunResult>,
    pub deduped_from: u32,
}

pub const FINDINGS_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "findings": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "path": { "type": "string" },
          "line": { "type": "integer" },
          "start_line": { "type": "integer" },
          "severity": { "type": "string" },
          "title": { "type": "string" },
          "explanation": { "type": "string" },
          "proposed_fix": { "type": "string" },
          "fix_options": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["path", "line", "title"]
      }
    }
  },
  "required": ["findings"]
}"#;

pub fn run_headless(
    client: &DetectedClient,
    model: &str,
    cwd: &Path,
    prompt: &str,
    kind: HeadlessKind,
) -> Result<(String, String, i32)> {
    let prompt_path = temp_artifact_path("scrutiny", "agent", "prompt");
    {
        let mut f = fs::File::create(&prompt_path)?;
        f.write_all(prompt.as_bytes())?;
    }

    let mut cmd = Command::new(&client.binary);
    cmd.current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match client.client.as_str() {
        "cursor" => {
            cmd.arg("-p")
                .arg("--trust")
                .arg("--output-format")
                .arg("json")
                .arg("--model")
                .arg(model)
                .arg("--workspace")
                .arg(cwd);
            match kind {
                HeadlessKind::Isolated | HeadlessKind::Ask => {
                    cmd.arg("--mode").arg("ask");
                }
                HeadlessKind::TeamLead => {}
            }
            cmd.arg(prompt);
        }
        "claude" => {
            cmd.arg("-p").arg("--output-format").arg("json");
            match kind {
                HeadlessKind::Isolated | HeadlessKind::Ask => {
                    cmd.arg("--bare")
                        .arg("--allowedTools")
                        .arg("Read")
                        .arg("--json-schema")
                        .arg(FINDINGS_JSON_SCHEMA);
                }
                HeadlessKind::TeamLead => {
                    cmd.arg("--json-schema").arg(FINDINGS_JSON_SCHEMA);
                }
            }
            cmd.arg("--model").arg(model).arg(prompt);
        }
        "codex" => {
            cmd.arg("exec")
                .arg("--json")
                .arg("-m")
                .arg(model)
                .arg(prompt);
        }
        other => bail!("unsupported client {other}"),
    }

    eprintln!(
        "scrutiny: running {} ({}) kind={kind:?}",
        client.client,
        client.binary.display()
    );

    let output = cmd
        .output()
        .with_context(|| format!("spawn {}", client.binary.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(1);
    Ok((stdout, stderr, code))
}

pub fn parse_findings_json(raw: &str, role: &str) -> Result<Vec<AgentFinding>> {
    let text = extract_json_payload(raw)?;
    let v: Value = serde_json::from_str(&text).context("parse findings JSON")?;
    let arr = if let Some(a) = v.get("findings").and_then(|x| x.as_array()) {
        a.clone()
    } else if let Some(a) = v.as_array() {
        a.clone()
    } else if let Some(r) = v.get("result").and_then(|x| x.as_str()) {
        return parse_findings_json(r, role);
    } else if let Some(r) = v.get("structured_output") {
        return parse_findings_json(&r.to_string(), role);
    } else {
        bail!("no findings array in agent output");
    };

    let mut out = Vec::new();
    for item in arr {
        let path = item
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string();
        let line = item.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32;
        if path.is_empty() || line == 0 {
            continue;
        }
        let sev = item
            .get("severity")
            .and_then(|s| s.as_str())
            .unwrap_or("warning");
        out.push(AgentFinding {
            path,
            line,
            start_line: item
                .get("start_line")
                .and_then(|s| s.as_u64())
                .map(|u| u as u32),
            severity: normalize_severity(sev),
            title: item
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("Untitled")
                .to_string(),
            explanation: item
                .get("explanation")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            proposed_fix: item
                .get("proposed_fix")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            fix_options: item
                .get("fix_options")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            source_role: role.to_string(),
        });
    }
    Ok(out)
}

fn extract_json_payload(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Ok(trimmed.to_string());
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = after
            .strip_prefix("json")
            .or_else(|| after.strip_prefix("JSON"))
            .unwrap_or(after);
        if let Some(end) = after.find("```") {
            return Ok(after[..end].trim().to_string());
        }
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if let Some(r) = v.get("result").and_then(|x| x.as_str()) {
            return extract_json_payload(r);
        }
        return Ok(trimmed.to_string());
    }
    bail!("could not extract JSON from agent stdout");
}

pub fn build_isolated_prompt(
    role: &str,
    pack_path: &Path,
    paths: &[String],
    plan: &ConfirmedPlan,
) -> String {
    let paths_list = if paths.is_empty() {
        "(entire pack)".into()
    } else {
        paths
            .iter()
            .map(|p| format!("- `{p}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let focus = match role {
        "security" => "ONLY security: auth, injection, secrets, access control.",
        "performance" => "ONLY performance: N+1, hot loops, waste work, memory.",
        "error_handling" => "ONLY error handling: swallowed errors, missing checks, bad retries.",
        "evangelist" => "Architecture / pattern consistency across change.",
        _ => "General review on assigned paths.",
    };
    format!(
        r#"Scrutiny {role} specialist. ISOLATED mode. No subagents. Pack (+ paths) only.

STYLE (mandatory):
- Load + follow **caveman skill** if present on this machine (skill `name: caveman`, invoke `/caveman ultra` or equivalent).
- Intensity: **ultra**. Terse. No fluff. No filler. Substance stay. Normal pronouns (`I`/`you`).
- title / explanation / proposed_fix / fix_options text: caveman ultra too.
- Never announce style. Never add "Caveman:" wrapper.

Pack: `{pack}`
Paths:
{paths_list}

Analyses on: security={} performance={} error_handling={}
Focus: {focus}

Output: JSON ONLY. No prose outside JSON.
{{"findings":[{{"path":"rel/path","line":1,"severity":"critical|warning|info","title":"...","explanation":"...","proposed_fix":"...","fix_options":[]}}]}}

Rules:
- Every finding: path + line (1-based) from pack/head.
- Nothing: {{"findings":[]}}
- Severity: critical|warning|info
"#,
        plan.security,
        plan.performance,
        plan.error_handling,
        pack = pack_path.display(),
        role = role,
        paths_list = paths_list,
        focus = focus,
    )
}

pub fn build_team_lead_prompt(pack_path: &Path, plan: &ConfirmedPlan) -> String {
    format!(
        r#"Scrutiny lead. TEAM mode. You spawn team. You collate. You own final report.

STYLE (mandatory):
- Load + follow **caveman skill** if present on this machine (skill `name: caveman`, invoke `/caveman ultra` or equivalent).
- Intensity: **ultra**. Terse. No fluff. Substance stay. Normal pronouns (`I`/`you`).
- Brief team in caveman ultra. Finding text (title/explanation/proposed_fix) caveman ultra.
- Never announce style.

Pack: `{pack}`
Team size guide:
- reviewers: {}
- evangelists: {}
- security specialist: {}
- performance specialist: {}
- error-handling specialist: {}

You do:
1. Spawn team (or equal parallel work)
2. Collect findings
3. Dedupe
4. Return ONE final JSON

Output: JSON ONLY.
{{"findings":[{{"path":"rel/path","line":1,"severity":"critical|warning|info","title":"...","explanation":"...","proposed_fix":"...","fix_options":[]}}]}}

Every finding: path + line. Clean: {{"findings":[]}}.
"#,
        plan.reviewers,
        plan.evangelists,
        plan.security,
        plan.performance,
        plan.error_handling,
        pack = pack_path.display(),
    )
}

pub fn build_ask_prompt(context: &str, question: &str) -> String {
    format!(
        "Clarify code-review finding.\n\n\
         STYLE (mandatory): load + follow **caveman skill** if present (skill `name: caveman`, `/caveman ultra`). \
         Intensity ultra. Terse. No fluff. Substance stay. Never announce style.\n\n\
         Context:\n{context}\n\n\
         Question:\n{question}\n"
    )
}

/// Run isolated parallel specialists; collate + dedupe into ReviewReport.
pub fn run_isolated_review(
    client: &DetectedClient,
    plan: &ConfirmedPlan,
    pack_path: &Path,
    cwd: &Path,
) -> Result<(ReviewReport, PathBuf)> {
    let mut jobs: Vec<(String, u32, Vec<String>)> = Vec::new();

    let buckets = if plan.reviewers > 0 {
        partition_pack_paths(pack_path, plan.reviewers)?
    } else {
        Vec::new()
    };
    for (i, paths) in buckets.into_iter().enumerate() {
        jobs.push(("reviewer".into(), (i + 1) as u32, paths));
    }
    if plan.evangelists > 0 {
        for i in 0..plan.evangelists {
            jobs.push(("evangelist".into(), i + 1, Vec::new()));
        }
    }
    if plan.security {
        jobs.push(("security".into(), 1, Vec::new()));
    }
    if plan.performance {
        jobs.push(("performance".into(), 1, Vec::new()));
    }
    if plan.error_handling {
        jobs.push(("error_handling".into(), 1, Vec::new()));
    }

    if jobs.is_empty() {
        bail!("isolated mode: no agents to spawn (reviewers/evangelists/specialists all off)");
    }

    let (tx, rx) = mpsc::channel();
    let job_count = jobs.len();
    for (role, index, paths) in jobs {
        let tx = tx.clone();
        let client = client.clone();
        let model = plan.model.clone();
        let pack = pack_path.to_path_buf();
        let cwd = cwd.to_path_buf();
        let plan_c = plan.clone();
        thread::spawn(move || {
            let prompt = build_isolated_prompt(&role, &pack, &paths, &plan_c);
            let result = match run_headless(
                &client,
                &model,
                &cwd,
                &prompt,
                HeadlessKind::Isolated,
            ) {
                Ok((stdout, stderr, code)) => {
                    let findings = if code == 0 {
                        parse_findings_json(&stdout, &role).unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    AgentRunResult {
                        role,
                        index,
                        paths,
                        findings,
                        ok: code == 0,
                        stderr,
                    }
                }
                Err(e) => AgentRunResult {
                    role,
                    index,
                    paths,
                    findings: Vec::new(),
                    ok: false,
                    stderr: format!("{e:#}"),
                },
            };
            let _ = tx.send(result);
        });
    }
    drop(tx);

    let mut agents = Vec::with_capacity(job_count);
    let deadline = std::time::Instant::now() + Duration::from_secs(60 * 45);
    while agents.len() < job_count {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            bail!("isolated review timed out waiting for agents");
        }
        match rx.recv_timeout(remaining) {
            Ok(r) => agents.push(r),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("isolated review timed out waiting for agents");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    for a in &agents {
        if !a.ok {
            eprintln!(
                "scrutiny: agent {}#{} failed: {}",
                a.role,
                a.index,
                a.stderr.lines().next().unwrap_or("(no stderr)")
            );
        }
    }

    let raw_count: u32 = agents.iter().map(|a| a.findings.len() as u32).sum();
    let mut all: Vec<AgentFinding> = agents.iter().flat_map(|a| a.findings.clone()).collect();
    let findings = dedupe_findings(&mut all);

    let report = ReviewReport {
        version: 1,
        spawn_mode: "isolated".into(),
        model: plan.model.clone(),
        findings,
        agents,
        deduped_from: raw_count,
    };
    let out = temp_artifact_path(&plan.client, "review", "report");
    write_json_pretty(&out, &report)?;
    Ok((report, out))
}

pub fn run_team_review(
    client: &DetectedClient,
    plan: &ConfirmedPlan,
    pack_path: &Path,
    cwd: &Path,
) -> Result<(ReviewReport, PathBuf)> {
    let prompt = build_team_lead_prompt(pack_path, plan);
    let (stdout, stderr, code) =
        run_headless(client, &plan.model, cwd, &prompt, HeadlessKind::TeamLead)?;
    if code != 0 {
        bail!("team lead agent failed (exit {code}): {stderr}");
    }
    let findings = parse_findings_json(&stdout, "lead")?;
    let agent = AgentRunResult {
        role: "lead".into(),
        index: 1,
        paths: Vec::new(),
        findings: findings.clone(),
        ok: true,
        stderr,
    };
    let report = ReviewReport {
        version: 1,
        spawn_mode: "team".into(),
        model: plan.model.clone(),
        findings,
        agents: vec![agent],
        deduped_from: 0,
    };
    let out = temp_artifact_path(&plan.client, "review", "report");
    write_json_pretty(&out, &report)?;
    Ok((report, out))
}

pub fn session_records_from_report(report: &ReviewReport) -> Vec<ReviewAgentRecord> {
    report
        .agents
        .iter()
        .map(|a| ReviewAgentRecord {
            role: a.role.clone(),
            index: a.index,
            paths: a.paths.clone(),
            findings_count: a.findings.len() as u32,
        })
        .collect()
}

fn severity_rank(s: &str) -> u8 {
    match normalize_severity(s).as_str() {
        "critical" => 3,
        "warning" => 2,
        _ => 1,
    }
}

/// Dedupe same path + nearby line + similar title; keep higher severity.
pub fn dedupe_findings(items: &mut [AgentFinding]) -> Vec<AgentFinding> {
    let mut out: Vec<AgentFinding> = Vec::new();
    for f in items.iter() {
        let mut merged = false;
        for existing in out.iter_mut() {
            if existing.path == f.path
                && existing.line.abs_diff(f.line) <= 2
                && title_similar(&existing.title, &f.title)
            {
                if severity_rank(&f.severity) > severity_rank(&existing.severity) {
                    *existing = f.clone();
                }
                merged = true;
                break;
            }
        }
        if !merged {
            out.push(f.clone());
        }
    }
    out
}

fn title_similar(a: &str, b: &str) -> bool {
    let na = normalize_title(a);
    let nb = normalize_title(b);
    if na == nb {
        return true;
    }
    na.contains(&nb) || nb.contains(&na)
}

fn normalize_title(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_nearby() {
        let mut items = vec![
            AgentFinding {
                path: "a.rs".into(),
                line: 10,
                start_line: None,
                severity: "warning".into(),
                title: "Missing check".into(),
                explanation: "".into(),
                proposed_fix: "".into(),
                fix_options: vec![],
                source_role: "reviewer".into(),
            },
            AgentFinding {
                path: "a.rs".into(),
                line: 11,
                start_line: None,
                severity: "critical".into(),
                title: "missing check".into(),
                explanation: "x".into(),
                proposed_fix: "".into(),
                fix_options: vec![],
                source_role: "security".into(),
            },
        ];
        let out = dedupe_findings(&mut items);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, "critical");
    }

    #[test]
    fn parse_findings_array() {
        let raw = r#"{"findings":[{"path":"x.ts","line":3,"title":"t","severity":"info"}]}"#;
        let f = parse_findings_json(raw, "reviewer").unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].line, 3);
    }
}
