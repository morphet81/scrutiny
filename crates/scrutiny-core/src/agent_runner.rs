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
    /// Forge implement / test-plan: full tools, no findings JSON schema.
    Forge,
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
    #[serde(default = "default_report_version")]
    pub version: u32,
    #[serde(default)]
    pub spawn_mode: String,
    #[serde(default)]
    pub model: String,
    pub findings: Vec<AgentFinding>,
    #[serde(default)]
    pub agents: Vec<AgentRunResult>,
    #[serde(default)]
    pub deduped_from: u32,
}

fn default_report_version() -> u32 {
    1
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

pub const AGENT_WALL_SECS: u64 = 10 * 60;
pub const PROGRESS_SECS: u64 = 15;

pub struct HeadlessOutcome {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub timed_out: bool,
}

pub fn run_headless(
    client: &DetectedClient,
    model: &str,
    cwd: &Path,
    prompt: &str,
    kind: HeadlessKind,
    label: &str,
    wall: Duration,
) -> Result<HeadlessOutcome> {
    let prompt_path = temp_artifact_path("scrutiny", "agent", "prompt");
    {
        let mut f = fs::File::create(&prompt_path)?;
        f.write_all(prompt.as_bytes())?;
    }

    let mut cmd = Command::new(&client.binary);
    // Null stdin: Claude -p otherwise waits ~3s for piped input.
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
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
                HeadlessKind::TeamLead | HeadlessKind::Forge => {}
            }
            cmd.arg(prompt);
        }
        "claude" => {
            // --bare skips OAuth/keychain (needs ANTHROPIC_API_KEY). Default: use login session.
            // SCRUTINY_CLAUDE_BARE=1 force bare; SCRUTINY_CLAUDE_NO_BARE=1 force OAuth even with API key.
            let use_bare = if std::env::var_os("SCRUTINY_CLAUDE_BARE").is_some() {
                true
            } else if std::env::var_os("SCRUTINY_CLAUDE_NO_BARE").is_some() {
                false
            } else {
                std::env::var_os("ANTHROPIC_API_KEY").is_some()
            };

            cmd.arg("-p").arg("--output-format").arg("json");
            if use_bare {
                cmd.arg("--bare");
            }
            match kind {
                HeadlessKind::Isolated | HeadlessKind::Ask => {
                    cmd.arg("--allowedTools")
                        .arg("Read")
                        .arg("--json-schema")
                        .arg(FINDINGS_JSON_SCHEMA);
                }
                HeadlessKind::TeamLead => {
                    cmd.arg("--json-schema").arg(FINDINGS_JSON_SCHEMA);
                }
                HeadlessKind::Forge => {
                    // Full tools; no findings schema (ticket implement / test plan).
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
        "scrutiny: start {label} via {} ({}) mode={kind:?}",
        client.client,
        client.binary.display()
    );

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", client.binary.display()))?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_h = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut r) = stdout_pipe {
            let _ = std::io::Read::read_to_string(&mut r, &mut buf);
        }
        buf
    });
    let stderr_h = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut r) = stderr_pipe {
            let _ = std::io::Read::read_to_string(&mut r, &mut buf);
        }
        buf
    });

    let started = std::time::Instant::now();
    let mut timed_out = false;
    let mut last_tick = started;
    let code = loop {
        match child.try_wait().context("wait agent child")? {
            Some(status) => break status.code().unwrap_or(1),
            None => {
                if started.elapsed() >= wall {
                    eprintln!(
                        "scrutiny: timeout {label} after {}s — killing",
                        wall.as_secs()
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break 124; // conventional timeout exit
                }
                if last_tick.elapsed() >= Duration::from_secs(PROGRESS_SECS) {
                    eprintln!(
                        "scrutiny: still running {label} ({}s)",
                        started.elapsed().as_secs()
                    );
                    last_tick = std::time::Instant::now();
                }
                thread::sleep(Duration::from_millis(400));
            }
        }
    };

    let stdout = stdout_h.join().unwrap_or_default();
    let mut stderr = stderr_h.join().unwrap_or_default();

    // Claude puts auth/API failures in stdout JSON (is_error), often with empty stderr.
    if let Some(msg) = claude_error_message(&stdout) {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&msg);
    }
    if timed_out {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "timed out after {}s — killed; parsing partial stdout if any",
            wall.as_secs()
        ));
    }

    if timed_out {
        eprintln!("scrutiny: done {label} (TIMEOUT)");
    } else if code == 0 {
        eprintln!("scrutiny: done {label} (ok)");
    } else {
        eprintln!("scrutiny: done {label} (exit {code})");
    }

    Ok(HeadlessOutcome {
        stdout,
        stderr,
        code,
        timed_out,
    })
}

/// Extract human-readable error from Claude `--output-format json` envelope.
fn claude_error_message(stdout: &str) -> Option<String> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let is_error = v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
    if !is_error {
        return None;
    }
    let result = v
        .get("result")
        .and_then(|x| x.as_str())
        .unwrap_or("claude reported is_error");
    Some(format!("claude: {result}"))
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
{{"findings":[{{"path":"rel/path","line":1,"severity":"critical|warning|suggestion","title":"...","explanation":"...","proposed_fix":"...","fix_options":[]}}]}}

Rules:
- Every finding: path + line (1-based).
- **CRITICAL:** `line` MUST be a new-side line present in that path's pack unified diff (added `+` preferred; context ` ` only if issue is clearly there). GitHub will reject out-of-diff lines — never invent them from the full file.
- If the issue is not on a PR/pack diff line → omit that finding.
- Nothing: {{"findings":[]}}
- Severity: critical|warning|suggestion
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
    let mut member_briefs = String::new();

    if plan.reviewers > 0 {
        let n = plan.reviewers;
        // Template: entire pack; lead assigns partitioned path lists when spawning each reviewer.
        let brief = build_isolated_prompt("reviewer", pack_path, &[], plan);
        member_briefs.push_str(&format!(
            "\n### reviewer × {n}\n\
             Spawn **exactly {n}** reviewer(s). Partition pack paths across them.\n\
             For each spawn: paste the template below VERBATIM, then replace the Paths section \
             with that reviewer's assigned paths only.\n\
             ```\n{brief}\n```\n"
        ));
    }

    if plan.evangelists > 0 {
        let n = plan.evangelists;
        let brief = build_isolated_prompt("evangelist", pack_path, &[], plan);
        member_briefs.push_str(&format!(
            "\n### evangelist × {n}\n\
             Spawn **exactly {n}** evangelist(s). Paste VERBATIM (entire pack):\n\
             ```\n{brief}\n```\n"
        ));
    }

    if plan.security {
        let brief = build_isolated_prompt("security", pack_path, &[], plan);
        member_briefs.push_str(&format!(
            "\n### security × 1\nPaste VERBATIM:\n```\n{brief}\n```\n"
        ));
    }
    if plan.performance {
        let brief = build_isolated_prompt("performance", pack_path, &[], plan);
        member_briefs.push_str(&format!(
            "\n### performance × 1\nPaste VERBATIM:\n```\n{brief}\n```\n"
        ));
    }
    if plan.error_handling {
        let brief = build_isolated_prompt("error_handling", pack_path, &[], plan);
        member_briefs.push_str(&format!(
            "\n### error_handling × 1\nPaste VERBATIM:\n```\n{brief}\n```\n"
        ));
    }

    if member_briefs.is_empty() {
        member_briefs.push_str(
            "\n(No member roles enabled — return {\"findings\":[]} or review pack yourself \
             using the same JSON rules.)\n",
        );
    }

    format!(
        r#"Scrutiny lead. TEAM mode. You spawn team. You collate. You own final report.

STYLE (mandatory):
- Load + follow **caveman skill** if present on this machine (skill `name: caveman`, invoke `/caveman ultra` or equivalent).
- Intensity: **ultra**. Terse. No fluff. Substance stay. Normal pronouns (`I`/`you`).
- Finding text in final JSON: caveman ultra. Never announce style.

Pack: `{pack}`
Team size (effective counts — honor exactly):
- reviewers: {}
- evangelists: {}
- security specialist: {}
- performance specialist: {}
- error-handling specialist: {}

## Member brief templates (MANDATORY)

Do **NOT** invent alternate system prompts for teammates.
When you spawn each member, the spawn message body MUST be the matching template below (verbatim), only adjusting the Paths section for reviewers as noted.
{member_briefs}

## Lead ops (mandatory)

1. Spawn **exactly** the counts above (parallel when possible).
2. Wait for **ALL** members to return a findings JSON array before consolidating. Status/idle/progress pings are NOT complete — re-request the JSON if missing.
3. Reject / re-ask any finding missing path+line, or whose line is not on that path's pack unified diff (GitHub-attachable). Pack-only: members must not fish outside pack / assigned paths.
4. Dedupe. On disagreement about the same issue, keep the **higher** severity (critical > warning > suggestion).
5. Return ONE final JSON on stdout (no prose outside JSON).

Output: JSON ONLY.
{{"findings":[{{"path":"rel/path","line":1,"severity":"critical|warning|suggestion","title":"...","explanation":"...","proposed_fix":"...","fix_options":[]}}]}}

Every finding: path + line on pack unified diff. Clean: {{"findings":[]}}.
"#,
        plan.reviewers,
        plan.evangelists,
        plan.security,
        plan.performance,
        plan.error_handling,
        pack = pack_path.display(),
        member_briefs = member_briefs,
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

/// Triage-time revise: return updated finding fields as JSON.
pub fn build_ask_revise_prompt(context: &str, question: &str) -> String {
    format!(
        "Revise code-review finding after reviewer question.\n\n\
         STYLE (mandatory): load + follow **caveman skill** if present (`/caveman ultra`). \
         Intensity ultra. Terse. Never announce style.\n\n\
         Context:\n{context}\n\n\
         Question:\n{question}\n\n\
         Output: JSON ONLY (no prose outside JSON):\n\
         {{\"title\":\"...\",\"explanation\":\"...\",\"proposed_fix\":\"...\",\"fix_options\":[],\
\"path\":\"rel/path\",\"line\":1}}\n\
         Rules:\n\
         - Keep or fix path+line so line is still on the PR/pack unified diff (GitHub-attachable).\n\
         - Prefer an added (+) line. Never invent out-of-diff lines.\n\
         - fix_options may be empty.\n"
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

    let wall = Duration::from_secs(AGENT_WALL_SECS);
    let pending: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(
            jobs.iter()
                .map(|(r, i, _)| format!("{r}#{i}"))
                .collect(),
        ));

    eprintln!(
        "scrutiny: spawning {} isolated agents (wall {}m): {}",
        jobs.len(),
        wall.as_secs() / 60,
        pending
            .lock()
            .map(|p| p.join(", "))
            .unwrap_or_default()
    );

    let (tx, rx) = mpsc::channel();
    let job_count = jobs.len();
    let batch_start = std::time::Instant::now();

    for (role, index, paths) in jobs {
        let tx = tx.clone();
        let client = client.clone();
        let model = plan.model.clone();
        let pack = pack_path.to_path_buf();
        let cwd = cwd.to_path_buf();
        let plan_c = plan.clone();
        let pending = pending.clone();
        let label = format!("{role}#{index}");
        let label_done = label.clone();
        thread::spawn(move || {
            let prompt = build_isolated_prompt(&role, &pack, &paths, &plan_c);
            let path_note = if paths.is_empty() {
                "entire pack".into()
            } else {
                format!("{} paths", paths.len())
            };
            eprintln!("scrutiny: agent {label} focus={path_note}");
            let result = match run_headless(
                &client,
                &model,
                &cwd,
                &prompt,
                HeadlessKind::Isolated,
                &label,
                wall,
            ) {
                Ok(out) => {
                    let auth_err = claude_error_message(&out.stdout);
                    let findings = parse_findings_json(&out.stdout, &role).unwrap_or_default();
                    let ok = (out.code == 0 && auth_err.is_none()) || !findings.is_empty();
                    let mut stderr = out.stderr;
                    if let Some(a) = auth_err {
                        if stderr.is_empty() {
                            stderr = a;
                        } else {
                            stderr = format!("{stderr}\n{a}");
                        }
                    }
                    AgentRunResult {
                        role,
                        index,
                        paths,
                        findings,
                        ok,
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
            if let Ok(mut p) = pending.lock() {
                p.retain(|x| x != &label_done);
            }
            let _ = tx.send(result);
        });
    }
    drop(tx);

    let mut agents = Vec::with_capacity(job_count);
    let mut last_progress = batch_start;
    while agents.len() < job_count {
        let remaining_wall = wall.saturating_sub(batch_start.elapsed());
        // Keep waiting a bit past wall so kill threads can flush/send.
        let grace = Duration::from_secs(30);
        let wait = remaining_wall
            .checked_add(grace)
            .unwrap_or(grace)
            .max(Duration::from_secs(1));
        match rx.recv_timeout(Duration::from_secs(PROGRESS_SECS).min(wait)) {
            Ok(r) => {
                let n = r.findings.len();
                eprintln!(
                    "scrutiny: collected {}#{} findings={n} ok={}",
                    r.role, r.index, r.ok
                );
                agents.push(r);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_progress.elapsed() >= Duration::from_secs(PROGRESS_SECS) {
                    let still = pending
                        .lock()
                        .map(|p| p.join(", "))
                        .unwrap_or_else(|_| "?".into());
                    eprintln!(
                        "scrutiny: in progress {}s — waiting: {}",
                        batch_start.elapsed().as_secs(),
                        if still.is_empty() {
                            "(flushing…)".into()
                        } else {
                            still
                        }
                    );
                    last_progress = std::time::Instant::now();
                }
                if batch_start.elapsed() > wall + grace {
                    eprintln!("scrutiny: batch wall exceeded — using {} finished agents", agents.len());
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Drain any late arrivals briefly
    while let Ok(r) = rx.try_recv() {
        agents.push(r);
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

    if agents.is_empty() {
        bail!("isolated review: no agent results (all stuck/killed with no output)");
    }

    if agents.iter().all(|a| !a.ok && a.findings.is_empty()) {
        let sample = agents
            .iter()
            .find_map(|a| {
                let s = a.stderr.trim();
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            })
            .unwrap_or_else(|| "all headless agents failed with empty stderr".into());
        bail!(
            "isolated review: every agent failed. First error: {sample}\n\
             Hint (claude): run `claude` once and /login, or set ANTHROPIC_API_KEY. \
             Do not use SCRUTINY_CLAUDE_BARE without an API key."
        );
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
    let wall = Duration::from_secs(AGENT_WALL_SECS);
    let out = run_headless(
        client,
        &plan.model,
        cwd,
        &prompt,
        HeadlessKind::TeamLead,
        "lead#1",
        wall,
    )?;
    if out.code != 0 && !out.timed_out {
        bail!("team lead agent failed (exit {}): {}", out.code, out.stderr);
    }
    let findings = parse_findings_json(&out.stdout, "lead").unwrap_or_default();
    if findings.is_empty() && out.code != 0 {
        bail!(
            "team lead produced no findings (exit {}): {}",
            out.code,
            out.stderr
        );
    }
    let agent = AgentRunResult {
        role: "lead".into(),
        index: 1,
        paths: Vec::new(),
        findings: findings.clone(),
        ok: out.code == 0 || !findings.is_empty(),
        stderr: out.stderr,
    };
    let report = ReviewReport {
        version: 1,
        spawn_mode: "team".into(),
        model: plan.model.clone(),
        findings,
        agents: vec![agent],
        deduped_from: 0,
    };
    let out_path = temp_artifact_path(&plan.client, "review", "report");
    write_json_pretty(&out_path, &report)?;
    Ok((report, out_path))
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

#[derive(Debug, Clone)]
pub struct AgentPromptInput {
    pub role: String,
    pub pack_path: PathBuf,
    pub plan_path: Option<PathBuf>,
    pub paths: Vec<String>,
}

/// Print isolated (or team-lead) prompt text for skill/debug paste.
pub fn run_agent_prompt(input: AgentPromptInput) -> Result<String> {
    let plan = if let Some(p) = &input.plan_path {
        let text = fs::read_to_string(p)
            .with_context(|| format!("read plan {}", p.display()))?;
        serde_json::from_str(&text).context("parse ConfirmedPlan")?
    } else {
        minimal_plan_for_prompt(&input.pack_path)
    };
    let role = input.role.trim().to_ascii_lowercase();
    let text = if role == "lead" || role == "team" || role == "team_lead" {
        build_team_lead_prompt(&input.pack_path, &plan)
    } else {
        build_isolated_prompt(&role, &input.pack_path, &input.paths, &plan)
    };
    Ok(text)
}

fn minimal_plan_for_prompt(pack_path: &Path) -> ConfirmedPlan {
    ConfirmedPlan {
        version: 1,
        client: "cursor".into(),
        model: "default".into(),
        security: true,
        performance: true,
        error_handling: true,
        reviewers: 1,
        evangelists: 1,
        reviewers_requested: 1,
        evangelists_requested: 1,
        skip_ai: false,
        skip_ai_reason: None,
        eval_path: String::new(),
        map_path: None,
        pack_path: Some(pack_path.display().to_string()),
        scan_path: None,
        max_reviewers: 4,
        spawn_evangelists: true,
        spawn_mode: "isolated".into(),
    }
}

fn severity_rank(s: &str) -> u8 {
    match normalize_severity(s).as_str() {
        "critical" => 3,
        "warning" => 2,
        _ => 1, // suggestion
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
    fn claude_error_from_stdout() {
        let raw = r#"{"type":"result","is_error":true,"result":"Not logged in · Please run /login"}"#;
        let msg = claude_error_message(raw).unwrap();
        assert!(msg.contains("Not logged in"));
    }

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
        let raw = r#"{"findings":[{"path":"x.ts","line":3,"title":"t","severity":"suggestion"}]}"#;
        let f = parse_findings_json(raw, "reviewer").unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].line, 3);
    }

    #[test]
    fn team_lead_embeds_isolated_reviewer_brief() {
        let plan = ConfirmedPlan {
            version: 1,
            client: "claude".into(),
            model: "sonnet".into(),
            security: true,
            performance: false,
            error_handling: false,
            reviewers: 2,
            evangelists: 1,
            reviewers_requested: 2,
            evangelists_requested: 1,
            skip_ai: false,
            skip_ai_reason: None,
            eval_path: String::new(),
            map_path: None,
            pack_path: Some("/tmp/pack.json".into()),
            scan_path: None,
            max_reviewers: 2,
            spawn_evangelists: true,
            spawn_mode: "team".into(),
        };
        let p = build_team_lead_prompt(Path::new("/tmp/pack.json"), &plan);
        assert!(p.contains("Member brief templates (MANDATORY)"));
        assert!(p.contains("Scrutiny reviewer specialist"));
        assert!(p.contains("Scrutiny evangelist specialist"));
        assert!(p.contains("Scrutiny security specialist"));
        assert!(!p.contains("Scrutiny performance specialist"));
        assert!(p.contains("higher") && p.contains("severity"));
        assert!(p.contains("Wait for **ALL** members"));
    }
}
