//! Headless agent CLI runner + finding collation for `scrutiny probe`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::paths::{artifact_path, artifact_path_unique, temp_artifact_path, write_json_pretty};
use crate::plan::ConfirmedPlan;
use crate::review_session::{partition_pack_paths, ReviewAgentRecord};
use crate::runtime::DetectedClient;
use crate::scan::normalize_severity;
use crate::terminal::{
    kill_cmd_for_terminal, launch_agent_in_surface, launch_agent_window, register_agent_pane,
    ItemSurface, ResolvedTerminal,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessKind {
    /// Read-focused specialist (no team spawn).
    Isolated,
    /// Consolidator: dedupe isolated findings, keep higher severity. Read-only + findings schema.
    Consolidate,
    /// Lead agent may spawn a team.
    TeamLead,
    /// Follow-up Q&A.
    Ask,
    /// Forge implement / test-plan: full tools, no findings JSON schema.
    Forge,
    /// Parley: fix PR review comments; full tools, no findings schema.
    Parley,
    /// One-shot free-form text (e.g. PR description). Read-only, no schema.
    Text,
    /// Probe PR overview (purpose, architecture, strengths, anchored concerns, review limits).
    /// Read-only + summary schema. Anchored concerns promoted to findings after merge.
    Summary,
}

/// Whether `model` supports Claude Code `--permission-mode auto`. Unsupported
/// models silently fall back to `default` (prompt on every action). Unsupported:
/// haiku (all), sonnet/opus 4.5, claude-3. Bare aliases (opus/sonnet/fable)
/// resolve to the latest model → supported.
pub fn model_supports_auto(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if m.contains("haiku") || m.contains("claude-3") {
        return false;
    }
    if (m.contains("opus") || m.contains("sonnet")) && (m.contains("4-5") || m.contains("4.5")) {
        return false;
    }
    true
}

/// Warn once per `(model, headless)` that a model does not support auto mode.
fn disclose_no_auto_once(model: &str, headless: bool) {
    use std::sync::OnceLock;
    static SEEN: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let key = format!("{model}|{headless}");
    if !seen.lock().expect("perm disclaimer lock").insert(key) {
        return;
    }
    if headless {
        eprintln!(
            "⚠ scrutiny: {model} does not support auto permission mode — running headless with \
             --dangerously-skip-permissions (bypasses all permission checks)."
        );
    } else {
        eprintln!(
            "⚠ scrutiny: {model} does not support auto permission mode — approve actions manually \
             in each agent pane."
        );
    }
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }

    pub fn add_assign(&mut self, other: &TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
    }
}

/// One headless Claude/Cursor call's usage, for bench aggregation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub usage: TokenUsage,
    #[serde(default)]
    pub wall_ms: u64,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub exit_code: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRunResult {
    pub role: String,
    pub index: u32,
    pub paths: Vec<String>,
    pub findings: Vec<AgentFinding>,
    pub ok: bool,
    pub stderr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
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
    /// Sum of per-agent usage when present (bench / instrumentation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_total: Option<TokenUsage>,
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

/// Ask-a-question follow-up: a direct `answer` for the reviewer plus optional
/// revised finding fields. Distinct from `FINDINGS_JSON_SCHEMA` — an ask reply
/// is one finding, not a `findings` array.
pub const PR_SUMMARY_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "purpose": { "type": "string" },
    "architecture": { "type": "string" },
    "good_points": { "type": "array", "items": { "type": "string" } },
    "bad_points": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "text": { "type": "string" },
          "path": { "type": "string" },
          "line": { "type": "integer" },
          "severity": { "type": "string" },
          "title": { "type": "string" },
          "proposed_fix": { "type": "string" }
        },
        "required": ["text", "path", "line"]
      }
    },
    "review_limits": { "type": "array", "items": { "type": "string" } },
    "diagram": { "type": "string" },
    "table": { "type": "string" }
  },
  "required": ["purpose", "architecture", "good_points", "bad_points", "review_limits"]
}"#;

/// Actionable concern from the PR overview agent. Anchored items feed the
/// consolidator (with reviewer findings) and remain eligible for triage promote.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SummaryConcern {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_fix: Option<String>,
}

impl SummaryConcern {
    /// True when path is non-empty and line > 0 — eligible for findings promote.
    pub fn is_actionable(&self) -> bool {
        self.path
            .as_ref()
            .map(|p| !p.trim().is_empty())
            .unwrap_or(false)
            && self.line.unwrap_or(0) > 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProbePrSummary {
    pub purpose: String,
    pub architecture: String,
    pub good_points: Vec<String>,
    pub bad_points: Vec<SummaryConcern>,
    /// Pack/tool/review-process caveats — display only, never findings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_limits: Vec<String>,
    /// Optional mermaid/ascii diagram — only when prose cannot show structure.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub diagram: String,
    /// Optional markdown table — only when a comparison needs columns.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub table: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrSummarySession {
    pub version: u32,
    pub summary: ProbePrSummary,
    pub model: String,
    pub client: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
}

pub const ASK_REVISE_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "answer": { "type": "string" },
    "title": { "type": "string" },
    "explanation": { "type": "string" },
    "proposed_fix": { "type": "string" },
    "fix_options": { "type": "array", "items": { "type": "string" } },
    "path": { "type": "string" },
    "line": { "type": "integer" }
  },
  "required": ["answer"]
}"#;

/// Shared `fix_options` + `explanation` policy (English).
const FIX_OPTIONS_POLICY_EN: &str = "\
- `fix_options`: default EMPTY. Put single best fix in `proposed_fix`.\n\
- Fill `fix_options` ONLY when 2+ real, materially-different approaches with true tradeoffs. NEVER exactly 1 option.\n\
- If filled: first entry = AI-preferred; its text say WHY preferred. Other entries say their tradeoff.\n\
- `explanation` (the Why): ONE short simple sentence. State problem, not story. Terse — shown in full.";

/// Shared policy, caveman ultra dialect.
const FIX_OPTIONS_POLICY_CV: &str = "\
- `fix_options`: default EMPTY. Best fix → `proposed_fix`.\n\
- Fill `fix_options` ONLY when 2+ real different approaches + true tradeoffs. NEVER exactly 1.\n\
- If filled: first = AI-preferred; say WHY. Others: tradeoff.\n\
- `explanation`: ONE short sentence. Problem, not story.";

/// Shared `fix_options` + `explanation` policy for findings prompts.
pub fn fix_options_policy() -> &'static str {
    crate::caveman::dialect(FIX_OPTIONS_POLICY_CV, FIX_OPTIONS_POLICY_EN)
}

/// Backward-compat alias (English text; prefer [`fix_options_policy`]).
pub const FIX_OPTIONS_POLICY: &str = FIX_OPTIONS_POLICY_EN;

/// Base agent wall, config-resolved. `[timeouts] agent_wall_secs` overrides it.
pub fn agent_wall_secs() -> u64 {
    crate::timeouts::get().agent
}

/// "still running" tick interval, config-resolved via `[timeouts] progress_secs`.
pub fn progress_secs() -> u64 {
    crate::timeouts::get().progress
}

pub struct HeadlessOutcome {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub timed_out: bool,
    pub usage: Option<TokenUsage>,
    pub request_id: Option<String>,
    pub session_id: Option<String>,
    pub wall_ms: u64,
}

/// When true, skip caveman inject (bench skill arm / config / env).
pub fn caveman_disabled() -> bool {
    !crate::caveman::caveman_enabled()
}

/// Optional preamble (full skill markdown) injected after caveman / before overrides.
pub fn bench_skill_preamble() -> Option<String> {
    let path = std::env::var_os("SCRUTINY_BENCH_SKILL_PREAMBLE")?;
    let text = fs::read_to_string(&path).ok()?;
    let t = text.trim();
    if t.is_empty() {
        None
    } else {
        Some(text)
    }
}

static USAGE_CAPTURE: std::sync::Mutex<Option<Vec<UsageRecord>>> = std::sync::Mutex::new(None);

/// Begin collecting [`UsageRecord`]s from every [`run_headless`] until [`take_usage_capture`].
pub fn start_usage_capture() {
    *USAGE_CAPTURE.lock().expect("usage capture lock") = Some(Vec::new());
}

/// Stop capture and return recorded calls (empty if capture was never started).
pub fn take_usage_capture() -> Vec<UsageRecord> {
    USAGE_CAPTURE
        .lock()
        .expect("usage capture lock")
        .take()
        .unwrap_or_default()
}

fn push_usage_record(rec: UsageRecord) {
    if let Ok(mut guard) = USAGE_CAPTURE.lock() {
        if let Some(buf) = guard.as_mut() {
            buf.push(rec);
        }
    }
}

/// Parse Claude `--output-format json` envelope usage (+ ids).
pub fn parse_claude_usage(stdout: &str) -> (Option<TokenUsage>, Option<String>, Option<String>) {
    let v: Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(_) => {
            // stream-json / trailing noise: try last JSON object line
            let Some(line) = stdout
                .lines()
                .rev()
                .find(|l| l.trim().starts_with('{') && l.contains("usage"))
            else {
                return (None, None, None);
            };
            match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => return (None, None, None),
            }
        }
    };
    let request_id = v
        .get("request_id")
        .or_else(|| v.get("requestId"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            v.get("uuid")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        });
    let session_id = v
        .get("session_id")
        .or_else(|| v.get("sessionId"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let usage = v.get("usage").and_then(|u| {
        Some(TokenUsage {
            input_tokens: u.get("input_tokens")?.as_u64().unwrap_or(0),
            output_tokens: u.get("output_tokens")?.as_u64().unwrap_or(0),
            cache_creation_input_tokens: u
                .get("cache_creation_input_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            cache_read_input_tokens: u
                .get("cache_read_input_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
        })
    });
    (usage, request_id, session_id)
}

/// Collapse duplicate records that share a request id (keep max output).
pub fn dedupe_usage_records(records: &[UsageRecord]) -> Vec<UsageRecord> {
    use std::collections::HashMap;
    let mut by_id: HashMap<String, UsageRecord> = HashMap::new();
    let mut no_id: Vec<UsageRecord> = Vec::new();
    for r in records {
        match &r.request_id {
            Some(id) if !id.is_empty() => {
                by_id
                    .entry(id.clone())
                    .and_modify(|prev| {
                        if r.usage.output_tokens > prev.usage.output_tokens {
                            *prev = r.clone();
                        }
                    })
                    .or_insert_with(|| r.clone());
            }
            _ => no_id.push(r.clone()),
        }
    }
    let mut out: Vec<_> = by_id.into_values().collect();
    out.extend(no_id);
    out
}

pub fn sum_usage_records(records: &[UsageRecord]) -> TokenUsage {
    let mut total = TokenUsage::default();
    for r in dedupe_usage_records(records) {
        total.add_assign(&r.usage);
    }
    total
}

/// Agent-type key derived from a spawn label: prefix before `#`, `-` → `_`.
/// e.g. `parley-member#1` → `parley_member`, `reviewer#3` → `reviewer`.
fn agent_type_from_label(label: &str) -> String {
    label
        .split('#')
        .next()
        .unwrap_or(label)
        .trim()
        .replace('-', "_")
}

/// Embedded caveman-ultra preamble (re-export for tests / callers).
pub use crate::caveman::{CAVEMAN_STYLE, CAVEMAN_ULTRA_PROMPT};

/// Prepend style / skill preamble / configured overrides to a prompt.
/// Order: caveman ultra (unless disabled) → bench skill preamble →
/// global/agent overrides → scrutiny's prompt.
pub fn inject_overrides(label: &str, prompt: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    // Summary must emit strict JSON — caveman preamble fights that contract.
    let skip_caveman = agent_type_from_label(label) == "summary";
    if crate::caveman::caveman_enabled() && !skip_caveman {
        parts.push(CAVEMAN_ULTRA_PROMPT.to_string());
    }
    if let Some(skill) = bench_skill_preamble() {
        parts.push(format!("# Skill context (mandatory)\n\n{skill}"));
    }
    let prefix = crate::config::resolve_prompt_prefix(&agent_type_from_label(label));
    if !prefix.is_empty() {
        parts.push(prefix);
    }
    parts.push(prompt.to_string());
    parts.join("\n\n")
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
    let injected = inject_overrides(label, prompt);
    let prompt = injected.as_str();
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
            if model.eq_ignore_ascii_case("auto") {
                eprintln!("scrutiny: cursor model auto — omitting --model (CLI default)");
            }
            for arg in cursor_headless_flags(model, cwd, kind) {
                cmd.arg(arg);
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
            // Unattended: auto mode where the model supports it, else bypass all
            // checks (headless can't approve interactively).
            if model_supports_auto(model) {
                cmd.arg("--permission-mode").arg("auto");
            } else {
                disclose_no_auto_once(model, true);
                cmd.arg("--dangerously-skip-permissions");
            }
            match kind {
                HeadlessKind::Isolated | HeadlessKind::Consolidate => {
                    cmd.arg("--allowedTools")
                        .arg("Read")
                        .arg("--json-schema")
                        .arg(FINDINGS_JSON_SCHEMA);
                }
                HeadlessKind::Ask => {
                    cmd.arg("--allowedTools")
                        .arg("Read")
                        .arg("--json-schema")
                        .arg(ASK_REVISE_JSON_SCHEMA);
                }
                HeadlessKind::Summary => {
                    cmd.arg("--allowedTools")
                        .arg("Read")
                        .arg("--json-schema")
                        .arg(PR_SUMMARY_JSON_SCHEMA);
                }
                HeadlessKind::TeamLead => {
                    cmd.arg("--json-schema").arg(FINDINGS_JSON_SCHEMA);
                }
                HeadlessKind::Text => {
                    cmd.arg("--allowedTools").arg("Read");
                }
                HeadlessKind::Forge | HeadlessKind::Parley => {
                    // Full tools; no findings schema (implement / address comments).
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
    let got_stdout = Arc::new(AtomicBool::new(false));
    let got_stdout_flag = got_stdout.clone();
    let stdout_h = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut r) = stdout_pipe {
            let mut chunk = [0u8; 8192];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        got_stdout_flag.store(true, Ordering::Relaxed);
                        buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                    Err(_) => break,
                }
            }
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
    let mut no_output_kill = false;
    let mut last_tick = started;
    let first_output_secs = crate::timeouts::get().headless_first_output;
    let code = loop {
        match child.try_wait().context("wait agent child")? {
            Some(status) => break status.code().unwrap_or(1),
            None => {
                if applies_first_output_kill(&client.client)
                    && should_kill_for_no_stdout(
                        got_stdout.load(Ordering::Relaxed),
                        started.elapsed(),
                        first_output_secs,
                        wall,
                    )
                {
                    eprintln!(
                        "scrutiny: timeout {label} after {}s — no stdout from model `{model}` \
                         (headless likely hung / unsupported); killing",
                        first_output_secs
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    no_output_kill = true;
                    break 124;
                }
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
                if last_tick.elapsed() >= Duration::from_secs(progress_secs()) {
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
        if no_output_kill {
            stderr.push_str(&format!(
                "no stdout within {first_output_secs}s from model `{model}` — killed; \
                 set [agent_models] overrides or use a headless-safe model"
            ));
        } else {
            stderr.push_str(&format!(
                "timed out after {}s — killed; parsing partial stdout if any",
                wall.as_secs()
            ));
        }
    }

    if timed_out {
        eprintln!("scrutiny: done {label} (TIMEOUT)");
    } else if code == 0 {
        eprintln!("scrutiny: done {label} (ok)");
    } else {
        eprintln!("scrutiny: done {label} (exit {code})");
    }

    let wall_ms = started.elapsed().as_millis() as u64;
    let (usage, request_id, session_id) = parse_claude_usage(&stdout);
    if let Some(ref u) = usage {
        push_usage_record(UsageRecord {
            label: label.to_string(),
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            usage: u.clone(),
            wall_ms,
            timed_out,
            exit_code: code,
        });
    }

    Ok(HeadlessOutcome {
        stdout,
        stderr,
        code,
        timed_out,
        usage,
        request_id,
        session_id,
        wall_ms,
    })
}

/// Launch a claude/cursor agent in a visible terminal window and return the
/// completion-sentinel path the host should poll.
///
/// The agent writes its results to disk as usual; the host collects from disk
/// after the sentinel appears. On success the pane auto-closes (`--close-on-exit`
/// / kill-pane). Leftovers are force-closed via [`force_close_agent_panes`].
pub fn run_nonheadless(
    client: &DetectedClient,
    model: &str,
    cwd: &Path,
    prompt: &str,
    label: &str,
    ctx: &ResolvedTerminal,
) -> Result<PathBuf> {
    let kill_cmd = kill_cmd_for_terminal(ctx);
    let pid_path = artifact_path_unique("agent-pane-pid");
    let (sentinel, script_path) =
        build_agent_script(client, model, cwd, prompt, label, Some(&kill_cmd), &pid_path)?;
    register_agent_pane(label, pid_path);
    eprintln!("scrutiny: launch {label} in {ctx:?} window (auto mode)");
    launch_agent_window(ctx, label, &script_path)?;
    Ok(sentinel)
}

/// Like [`run_nonheadless`] but launches the agent into a per-item [`ItemSurface`]
/// as a pane named after its `role` (bulk mode). `close_on_exit=false` keeps the
/// pane open after a clean exit (dry mode).
pub fn run_nonheadless_in(
    client: &DetectedClient,
    model: &str,
    cwd: &Path,
    prompt: &str,
    role: &str,
    surface: &ItemSurface,
    close_on_exit: bool,
) -> Result<PathBuf> {
    let pid_path = artifact_path_unique("agent-pane-pid");
    let (sentinel, script_path) =
        build_agent_script(client, model, cwd, prompt, role, None, &pid_path)?;
    // Bulk/item surfaces manage their own lifetime; still track PID for host exit cleanup.
    register_agent_pane(role, pid_path);
    eprintln!("scrutiny: launch {role} into item surface (auto mode)");
    launch_agent_in_surface(surface, role, &script_path, close_on_exit)?;
    Ok(sentinel)
}

/// Dry mode: open a role-named pane that only prints what *would* run and stays
/// open (`close_on_exit=false`). No agent is spawned, no sentinel to wait on.
pub fn run_dry_placeholder_in(cwd: &Path, role: &str, surface: &ItemSurface) -> Result<()> {
    let script_path = artifact_path_unique("dry-script");
    let script = format!(
        "#!/usr/bin/env bash\ncd '{cwd}'\n\
         echo '[dry] would run {role} here — no agent spawned'\n\
         exec bash\n",
        cwd = cwd.display(),
    );
    fs::write(&script_path, script.as_bytes())
        .with_context(|| format!("write {}", script_path.display()))?;
    launch_agent_in_surface(surface, role, &script_path, false)
}

/// Write the agent prompt + launcher script; return `(sentinel, script_path)`.
///
/// `kill_cmd`: optional shell command to run when sentinel exists (non-headless mode).
/// Close decision is based on sentinel presence, not exit code — the agent may exit
/// non-zero even after successfully running the sentinel touch, and we must close the
/// pane in that case too. Failures (no sentinel) keep the pane briefly; the host
/// force-closes leftovers when agents finish or scrutiny exits.
///
/// The script writes its bash PID to `pid_path` so the host can SIGTERM the pane.
fn build_agent_script(
    client: &DetectedClient,
    model: &str,
    cwd: &Path,
    prompt: &str,
    label: &str,
    kill_cmd: Option<&str>,
    pid_path: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let sentinel = artifact_path_unique("agent-done");
    let _ = fs::remove_file(&sentinel); // clear any stale marker
    let _ = fs::remove_file(pid_path);

    let prompt_path = temp_artifact_path("scrutiny", "agent", "prompt");
    let base = inject_overrides(label, prompt);
    let full_prompt = format!(
        "{base}\n\n---\nWhen you are completely finished (all outputs written to disk), \
         run this shell command exactly once so the host knows you are done:\n\n\
         touch '{}'\n",
        sentinel.display()
    );
    fs::write(&prompt_path, full_prompt.as_bytes())
        .with_context(|| format!("write {}", prompt_path.display()))?;

    let invoke = nonheadless_invoke_line(&client.client, &client.binary, model, cwd, &prompt_path)?;

    let sentinel_str = sentinel.display().to_string();
    let pid_str = pid_path.display().to_string();
    let script_path = artifact_path_unique("agent-script");
    let script = if let Some(kill) = kill_cmd {
        format!(
            "#!/usr/bin/env bash\n\
             echo $$ > '{pid}'\n\
             cd '{cwd}'\n\
             {invoke}\n\
             if [ -f '{sentinel}' ]; then {kill}; exit 0; fi\n\
             echo \"scrutiny: agent '{label}' did not complete — pane kept until host exits\"\n\
             exec bash\n",
            cwd = cwd.display(),
            sentinel = sentinel_str,
            pid = pid_str,
            label = label,
        )
    } else {
        format!(
            "#!/usr/bin/env bash\n\
             echo $$ > '{pid}'\n\
             cd '{cwd}'\n\
             {invoke}\n\
             code=$?\n\
             if [ \"$code\" -eq 0 ]; then exit 0; fi\n\
             echo \"scrutiny: agent '{label}' failed (exit $code); pane kept until host exits\"\n\
             exec bash\n",
            cwd = cwd.display(),
            pid = pid_str,
            label = label,
        )
    };
    fs::write(&script_path, script.as_bytes())
        .with_context(|| format!("write {}", script_path.display()))?;
    Ok((sentinel, script_path))
}

/// Shell command that starts the agent in a visible pane (no shebang).
pub(crate) fn nonheadless_invoke_line(
    client: &str,
    binary: &Path,
    model: &str,
    cwd: &Path,
    prompt_path: &Path,
) -> Result<String> {
    let binary = binary.display();
    let prompt = prompt_path.display();
    match client {
        "claude" => {
            let perm_flag = if model_supports_auto(model) {
                "--permission-mode auto"
            } else {
                disclose_no_auto_once(model, false);
                "--permission-mode default"
            };
            Ok(format!(
                "'{binary}' {perm_flag} --model '{model}' \"$(cat '{prompt}')\""
            ))
        }
        "cursor" => {
            let cwd = cwd.display();
            Ok(format!(
                "'{binary}' --trust --force --model '{model}' --workspace '{cwd}' \"$(cat '{prompt}')\""
            ))
        }
        other => bail!("non-headless mode supports claude and cursor only (got {other})"),
    }
}

/// Headless Cursor flags after the binary (prompt is appended by the caller).
/// `--model auto` is omitted: Cursor `-p` has no documented Auto picker.
pub(crate) fn cursor_headless_flags(model: &str, cwd: &Path, kind: HeadlessKind) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        "--trust".into(),
        "--output-format".into(),
        "json".into(),
    ];
    if !model.eq_ignore_ascii_case("auto") {
        args.push("--model".into());
        args.push(model.to_string());
    }
    args.push("--workspace".into());
    args.push(cwd.display().to_string());
    match kind {
        HeadlessKind::Isolated
        | HeadlessKind::Consolidate
        | HeadlessKind::Ask
        | HeadlessKind::Summary
        | HeadlessKind::Text => {
            args.push("--mode".into());
            args.push("ask".into());
        }
        HeadlessKind::TeamLead | HeadlessKind::Forge | HeadlessKind::Parley => {}
    }
    args
}

/// Poll until every sentinel file exists or the wall clock elapses.
/// Returns the sentinels that never appeared (empty = all agents finished).
pub fn wait_for_sentinels(sentinels: &[PathBuf], wall: Duration) -> Vec<PathBuf> {
    static NEVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    wait_for_sentinels_cancellable(sentinels, wall, &NEVER)
}

/// Like [`wait_for_sentinels`] but returns early (with the still-missing set)
/// when `cancel` flips true — used by bulk `q` abort.
pub fn wait_for_sentinels_cancellable(
    sentinels: &[PathBuf],
    wall: Duration,
    cancel: &std::sync::atomic::AtomicBool,
) -> Vec<PathBuf> {
    use std::sync::atomic::Ordering;
    let start = std::time::Instant::now();
    let mut last_tick = start;
    loop {
        let missing: Vec<PathBuf> = sentinels.iter().filter(|s| !s.exists()).cloned().collect();
        if missing.is_empty() {
            return Vec::new();
        }
        if cancel.load(Ordering::Relaxed) {
            return missing;
        }
        if start.elapsed() >= wall {
            return missing;
        }
        if last_tick.elapsed() >= Duration::from_secs(progress_secs()) {
            eprintln!(
                "scrutiny: waiting on {} agent window(s) ({}s)",
                missing.len(),
                start.elapsed().as_secs()
            );
            last_tick = std::time::Instant::now();
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// Whether to kill a headless child that has produced no stdout yet.
/// `first_output_secs == 0` disables. Cap at `wall` so wall timeout still wins.
/// Cursor skips this kill at the spawn loop (JSON is buffered until done).
pub fn should_kill_for_no_stdout(
    got_stdout: bool,
    elapsed: Duration,
    first_output_secs: u64,
    wall: Duration,
) -> bool {
    if got_stdout || first_output_secs == 0 {
        return false;
    }
    let limit = Duration::from_secs(first_output_secs).min(wall);
    elapsed >= limit
}

/// Cursor `--output-format json` buffers until done — empty stdout is normal.
/// Wall timeout still applies.
pub(crate) fn applies_first_output_kill(client: &str) -> bool {
    client != "cursor"
}

/// Client-specific login hint when every isolated headless agent fails.
pub(crate) fn isolated_all_failed_hint(client: &str) -> &'static str {
    match client {
        "cursor" => "Hint (cursor): run `agent login`, or set CURSOR_API_KEY.",
        "codex" => "Hint (codex): run `codex login` or check Codex auth.",
        _ => {
            "Hint (claude): run `claude` once and /login, or set ANTHROPIC_API_KEY. \
             Do not use SCRUTINY_CLAUDE_BARE without an API key."
        }
    }
}

pub(crate) fn claude_error_message(stdout: &str) -> Option<String> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let is_error = v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
    if !is_error {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(result) = v.get("result").and_then(|x| x.as_str()) {
        if !result.is_empty() && result != "claude reported is_error" {
            parts.push(result.to_string());
        }
    }
    if let Some(reason) = v.get("terminal_reason").and_then(|x| x.as_str()) {
        if !reason.is_empty() {
            parts.push(format!("terminal_reason={reason}"));
        }
    }
    if let Some(subtype) = v.get("subtype").and_then(|x| x.as_str()) {
        if !subtype.is_empty() && subtype != "success" {
            parts.push(format!("subtype={subtype}"));
        }
    }
    if let Some(errs) = v.get("errors").and_then(|x| x.as_array()) {
        for e in errs {
            if let Some(s) = e.as_str() {
                if !s.is_empty() {
                    parts.push(s.to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        parts.push("claude reported is_error".into());
    }
    Some(format!("claude: {}", parts.join("; ")))
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
        crate::caveman::dialect("(entire pack)", "(entire pack)").into()
    } else {
        paths
            .iter()
            .map(|p| format!("- `{p}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let focus = match role {
        "security" => crate::caveman::dialect(
            "ONLY security: auth, injection, secrets, access control.",
            "ONLY security: auth, injection, secrets, access control.",
        ),
        "performance" => crate::caveman::dialect(
            "ONLY performance: N+1, hot loops, waste work, memory.",
            "ONLY performance: N+1, hot loops, waste work, memory.",
        ),
        "error_handling" => crate::caveman::dialect(
            "ONLY error handling: swallowed errors, missing checks, bad retries.",
            "ONLY error handling: swallowed errors, missing checks, bad retries.",
        ),
        "evangelist" => crate::caveman::dialect(
            "Architecture / pattern consistency across change.",
            "Architecture / pattern consistency across the change.",
        ),
        _ => crate::caveman::dialect(
            "General review on assigned paths.",
            "General review on assigned paths.",
        ),
    };
    let header = format!("Scrutiny {role} specialist. ISOLATED mode. No subagents.");
    let body = crate::caveman::dialect(
        r#"## Context policy (save tokens)

Tier 0 (default): pack only — diffs, symbol slices, annex, outlined names, referenced_signatures.
Tier 1: pack lists `dropped_regions[].fetch_cmd` or `explore.allowed_paths` → MAY Read that path (or exact fetch_cmd). Prefer Read over Bash.
Tier 2: at most 6 extra Reads of head files already in pack/xref/imports. No whole-repo rg/find. No writes.
Stop at quota. No explore "to get oriented." Finding `line` MUST sit on that path's pack unified diff (GitHub-attachable). Explore = understanding only.

Pack shape (v2):
- `manifest[]`: changed-file outline; `dropped_regions` may sit in annex.
- `referenced_signatures[]`: cross-file defs (may include short body slice).
- Locale/i18n files NOT for AI review — deterministic scan owns them.

Analyses on: security={sec} performance={perf} error_handling={err}
Focus: {focus}

Output: JSON ONLY. No prose outside JSON.
{{"findings":[{{"path":"rel/path","line":1,"severity":"critical|warning|suggestion","title":"...","explanation":"...","proposed_fix":"...","fix_options":[]}}]}}

Rules:
- Every finding: path + line (1-based).
- **CRITICAL:** `line` MUST be **changed** line in that path's pack unified diff — added `+` (RIGHT) preferred, or deleted `-` (LEFT). Never context ` ` line. GitHub reject out-of-diff — never invent from full file.
- Issue only on unchanged context line → omit finding or file-level (omit `line`).
- Nothing: {{"findings":[]}}
- Severity: critical|warning|suggestion
{policy}

## Pack (your data)
Pack: `{pack}`
Prefer pack.md sibling if present (same stem). Paths:
{paths_list}
"#,
        r#"## Context policy (graduated — save tokens)

Tier 0 (default): use pack only — diffs, symbol slices, annex, outlined names, referenced_signatures.
Tier 1: if pack lists `dropped_regions[].fetch_cmd` or `explore.allowed_paths`, you MAY Read that path (or run that exact fetch_cmd). Prefer Read over Bash.
Tier 2: at most 6 extra Reads of head files whose path/symbol already appears in pack/xref/imports. No whole-repo rg/find. No writes.
Stop when quota hit. Do NOT explore "to get oriented." Finding `line` MUST still be on that path's pack unified diff (GitHub-attachable). Exploration is for understanding only.

Pack shape (v2):
- `manifest[]`: outline of changed files; `dropped_regions` may already be inlined in annex.
- `referenced_signatures[]`: cross-file defs (may include short body slice).
- Locale/i18n files are NOT for AI review — deterministic scan owns them.

Analyses on: security={sec} performance={perf} error_handling={err}
Focus: {focus}

Output: JSON ONLY. No prose outside JSON.
{{"findings":[{{"path":"rel/path","line":1,"severity":"critical|warning|suggestion","title":"...","explanation":"...","proposed_fix":"...","fix_options":[]}}]}}

Rules:
- Every finding: path + line (1-based).
- **CRITICAL:** `line` MUST be a **changed** line in that path's pack unified diff — added `+` (new-side, RIGHT) preferred, or deleted `-` (old-side, LEFT). Never a context ` ` line. GitHub will reject out-of-diff lines — never invent them from the full file.
- If the issue is only on an unchanged context line → omit that finding or attach at file level (omit `line`).
- Nothing: {{"findings":[]}}
- Severity: critical|warning|suggestion
{policy}

## Pack (your data)
Pack: `{pack}`
Prefer pack.md sibling if present (same stem). Paths:
{paths_list}
"#,
    );
    // Templates use `{…}` placeholders; double-brace JSON stays as `{{` until one replace pass.
    let filled = body
        .replace("{pack}", &pack_path.display().to_string())
        .replace("{paths_list}", &paths_list)
        .replace("{sec}", &plan.security.to_string())
        .replace("{perf}", &plan.performance.to_string())
        .replace("{err}", &plan.error_handling.to_string())
        .replace("{focus}", focus)
        .replace("{policy}", fix_options_policy())
        .replace("{{", "{")
        .replace("}}", "}");
    format!("{header}\n\n{filled}")
}

pub fn build_team_lead_prompt(pack_path: &Path, plan: &ConfirmedPlan) -> String {
    let mut member_briefs = String::new();
    let spawn_reviewers = crate::caveman::dialect(
        "Spawn **exactly {n}** reviewer(s). Partition pack paths across them.\n\
         For each spawn: paste template below VERBATIM, then replace Paths section \
         with that reviewer's assigned paths only.\n",
        "Spawn **exactly {n}** reviewer(s). Partition pack paths across them.\n\
         For each spawn: paste the template below VERBATIM, then replace the Paths section \
         with that reviewer's assigned paths only.\n",
    );
    let spawn_evangelists = crate::caveman::dialect(
        "Spawn **exactly {n}** evangelist(s). Paste VERBATIM (entire pack):\n",
        "Spawn **exactly {n}** evangelist(s). Paste VERBATIM (entire pack):\n",
    );

    if plan.reviewers > 0 {
        let n = plan.reviewers;
        let brief = build_isolated_prompt("reviewer", pack_path, &[], plan);
        let howto = spawn_reviewers.replace("{n}", &n.to_string());
        member_briefs.push_str(&format!("\n### reviewer × {n}\n{howto}```\n{brief}\n```\n"));
    }

    if plan.evangelists > 0 {
        let n = plan.evangelists;
        let brief = build_isolated_prompt("evangelist", pack_path, &[], plan);
        let howto = spawn_evangelists.replace("{n}", &n.to_string());
        member_briefs.push_str(&format!(
            "\n### evangelist × {n}\n{howto}```\n{brief}\n```\n"
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
        member_briefs.push_str(crate::caveman::dialect(
            "\n(No member roles enabled — return {\"findings\":[]} or review pack yourself \
             using same JSON rules.)\n",
            "\n(No member roles enabled — return {\"findings\":[]} or review pack yourself \
             using the same JSON rules.)\n",
        ));
    }

    let header = crate::caveman::dialect(
        "Scrutiny lead. TEAM mode. You spawn team. You collate. You own final report.",
        "Scrutiny lead. TEAM mode. You spawn the team. You collate. You own the final report.",
    );
    let ops = crate::caveman::dialect(
        r#"## Lead ops (mandatory)

1. Spawn **exactly** counts above (parallel when possible).
2. Wait for **ALL** members → findings JSON array before consolidate. Status/idle/progress pings NOT complete — re-request JSON if missing.
3. Reject / re-ask finding missing path+line, or line not **changed** (added `+` or deleted `-`) in that path's pack unified diff. Context ` ` lines bad. Prefer pack+annex; bounded Tier-1/2 explore only (see member templates). No whole-repo fishing.
4. Dedupe. Disagreement on same issue → keep **higher** severity (critical > warning > suggestion).
5. Members may use pack annex / allowlisted fetch for omitted bodies; finding lines STILL only on changed lines in path unified_diff.
6. Return ONE final JSON on stdout (no prose outside JSON)."#,
        r#"## Lead ops (mandatory)

1. Spawn **exactly** the counts above (parallel when possible).
2. Wait for **ALL** members to return a findings JSON array before consolidating. Status/idle/progress pings are NOT complete — re-request the JSON if missing.
3. Reject / re-ask any finding missing path+line, or whose line is not a **changed** line (added `+` or deleted `-`) in that path's pack unified diff. Context ` ` lines are not acceptable. Prefer pack+annex; bounded Tier-1/2 exploration only (see member templates). No whole-repo fishing.
4. Dedupe. On disagreement about the same issue, keep the **higher** severity (critical > warning > suggestion).
5. Members may use pack annex / allowlisted fetch for omitted bodies; finding lines STILL only on changed lines (added `+` or deleted `-`) in the path unified_diff.
6. Return ONE final JSON on stdout (no prose outside JSON)."#,
    );

    format!(
        r#"{header}

{ops}

Output: JSON ONLY.
{{"findings":[{{"path":"rel/path","line":1,"severity":"critical|warning|suggestion","title":"...","explanation":"...","proposed_fix":"...","fix_options":[]}}]}}

Every finding: path + line on pack unified diff. Clean: {{"findings":[]}}.
{policy}

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
"#,
        plan.reviewers,
        plan.evangelists,
        plan.security,
        plan.performance,
        plan.error_handling,
        header = header,
        pack = pack_path.display(),
        member_briefs = member_briefs,
        ops = ops,
        policy = fix_options_policy(),
    )
}

/// Consolidation agent prompt. Input = raw findings from reviewers/specialists
/// plus anchored PR-summary concerns; output = same JSON shape, semantically deduped.
pub fn build_consolidation_prompt(findings_json: &str, pack_path: &Path) -> String {
    let header = crate::caveman::dialect(
        "Scrutiny consolidator. Input = raw findings from reviewers + PR-summary concerns. You dedupe. You do not review.",
        "Scrutiny consolidator. Input = raw findings from reviewers and PR-summary concerns. You dedupe. You do not review.",
    );
    let rules = crate::caveman::dialect(
        r#"## Rules (mandatory)

1. Merge findings = **same issue**: same `path` + same/near `line`, OR same root cause even when titled differently (incl. `source_role` summary vs reviewer).
2. Duplicates differ `severity` → keep **higher**. Rank: critical > warning > suggestion.
3. Do **NOT** invent new findings. Do **NOT** change anchors. Do **NOT** drop unique finding. Consolidate only.
4. Every kept finding retain `path` + `line`.
5. Prefer clearest `title`/`explanation`/`proposed_fix` among merged duplicates.
6. Keep `source_role` when present (e.g. `summary`) on the surviving finding."#,
        r#"## Rules (mandatory)

1. Merge findings that describe the **same issue**: same `path` + same/near `line`, OR same root cause even when titled differently (including `source_role` summary vs reviewer).
2. On duplicates with differing `severity`, keep the **higher** one. Rank: critical > warning > suggestion.
3. Do **NOT** invent new findings. Do **NOT** change anchors. Do **NOT** drop a unique finding. Consolidate only.
4. Every kept finding must retain `path` + `line`.
5. Prefer the clearest `title`/`explanation`/`proposed_fix` among merged duplicates.
6. Preserve `source_role` when present (e.g. `summary`) on the surviving finding."#,
    );
    format!(
        r#"{header}

{rules}

## Output
JSON ONLY (no prose outside JSON):
{{"findings":[{{"path":"rel/path","line":1,"severity":"critical|warning|suggestion","title":"...","explanation":"...","proposed_fix":"...","fix_options":[]}}]}}

Nothing to merge → return the input findings unchanged. Empty input → {{"findings":[]}}.
{policy}

Pack (reference only — Read to disambiguate a duplicate if needed): `{pack}`

## Input findings
{findings}
"#,
        header = header,
        pack = pack_path.display(),
        rules = rules,
        findings = findings_json,
        policy = fix_options_policy(),
    )
}

/// Triage-time ask: answer the reviewer, and revise the finding only if the
/// answer actually changes it.
pub fn build_ask_revise_prompt(context: &str, question: &str) -> String {
    let intro = crate::caveman::dialect(
        "Answer reviewer question about code-review finding, then revise finding only if needed.",
        "Answer the reviewer question about a code-review finding, then revise the finding only if needed.",
    );
    let rules = crate::caveman::dialect(
        "Rules:\n\
         - `answer` REQUIRED: direct reply, 1-3 short sentences. Answer it, do not restate finding.\n\
         - Say plainly when reviewer right and finding wrong or moot — valid answer.\n\
         - All other fields OPTIONAL. OMIT every field answer does not change. \
           Unchanged finding = `answer` only.\n\
         - Include field only when new value differs from current in Context.\n\
         - path+line must point to **changed** line: added `+` (RIGHT) or deleted `-` (LEFT). Never context line.\n\
         - Prefer added (+) line. Never invent out-of-diff lines.\n",
        "Rules:\n\
         - `answer` is REQUIRED: direct reply to the question, 1-3 short sentences. Answer it, do not restate the finding.\n\
         - Say plainly when the reviewer is right and the finding is wrong or moot — that is a valid answer.\n\
         - All other fields are OPTIONAL. OMIT every field the answer does not change. \
           Unchanged finding = `answer` only.\n\
         - Include a field only when its new value differs from the current one shown in Context.\n\
         - path+line must point to a **changed** line: added `+` (RIGHT side) or deleted `-` (LEFT side). Never a context line.\n\
         - Prefer an added (+) line. Never invent out-of-diff lines.\n",
    );
    format!(
        "{intro}\n\n\
         Output: JSON ONLY (no prose outside JSON):\n\
         {{\"answer\":\"...\",\"title\":\"...\",\"explanation\":\"...\",\"proposed_fix\":\"...\",\"fix_options\":[],\
\"path\":\"rel/path\",\"line\":1}}\n\
         {rules}\
         {policy}\n\n\
         Context (includes file diff and code window — answer from it; Read only if strictly necessary):\n\
         {context}\n\n\
         Question:\n{question}\n",
        intro = intro,
        context = context,
        question = question,
        rules = rules,
        policy = fix_options_policy(),
    )
}

/// Append to a findings prompt: write JSON to disk instead of stdout.
/// Used in non-headless mode where stdout is not captured.
fn nonheadless_findings_suffix(out_path: &Path) -> String {
    format!(
        "\n\n---\nNON-HEADLESS OUTPUT OVERRIDE:\n\
         Do NOT print the findings JSON to stdout.\n\
         Instead, WRITE the findings JSON object (the same `{{\"findings\":[…]}}` you would \
         have printed) to this file (create/overwrite):\n\
           {}\n\
         Use the Write tool. The host reads that file after you finish.\n",
        out_path.display()
    )
}

/// Non-headless PR summary: write the overview JSON object to disk.
fn nonheadless_summary_suffix(out_path: &Path) -> String {
    format!(
        "\n\n---\nNON-HEADLESS OUTPUT OVERRIDE:\n\
         Do NOT print the overview JSON to stdout.\n\
         Instead, WRITE the overview JSON object (purpose/architecture/good_points/\
         bad_points/review_limits/diagram/table) to this file (create/overwrite):\n\
           {}\n\
         Use the Write tool. The host reads that file after you finish.\n\
         JSON ONLY in that file — no markdown fences, no prose.\n",
        out_path.display()
    )
}

fn dump_pr_summary_raw(client: &str, raw: &str) -> PathBuf {
    let dump = temp_artifact_path(client, "review", "pr-summary-raw");
    let _ = fs::write(&dump, raw.as_bytes());
    dump
}

fn finalize_pr_summary(
    summary: ProbePrSummary,
    client: &DetectedClient,
    model: &str,
    session_id: Option<String>,
    request_id: Option<String>,
    wall_ms: Option<u64>,
) -> Result<(ProbePrSummary, PathBuf)> {
    if !summary_has_content(&summary) {
        bail!("pr summary agent returned empty purpose and architecture");
    }
    let session = PrSummarySession {
        version: 1,
        summary: summary.clone(),
        model: model.to_string(),
        client: client.client.clone(),
        session_id,
        request_id,
        wall_ms,
    };
    let out_path = temp_artifact_path(&client.client, "review", "pr-summary");
    write_json_pretty(&out_path, &session)?;
    Ok((summary, out_path))
}

fn parse_pr_summary_raw(client: &str, raw: &str) -> Result<ProbePrSummary> {
    match parse_pr_summary_json(raw) {
        Ok(s) if summary_has_content(&s) => Ok(s),
        Ok(_) => {
            let dump = dump_pr_summary_raw(client, raw);
            bail!(
                "pr summary agent returned empty purpose and architecture (raw → {})",
                dump.display()
            )
        }
        Err(e) => {
            let dump = dump_pr_summary_raw(client, raw);
            bail!("{e:#} (raw → {})", dump.display())
        }
    }
}

/// Dedicated PR overview agent before findings triage.
/// Honors non-headless mode: visible pane + Write JSON to disk (same as reviewers).
/// Headless: `--json-schema` + parse stdout. One retry on parse failure either way.
pub fn run_pr_summary_agent(
    client: &DetectedClient,
    model: &str,
    pack_path: &Path,
    cwd: &Path,
    term: Option<&ResolvedTerminal>,
) -> Result<(ProbePrSummary, PathBuf)> {
    let prompt = build_pr_summary_prompt(pack_path);
    eprintln!("scrutiny probe: pr summary agent…");

    if let Some(ctx) = term {
        return run_pr_summary_nonheadless(client, model, cwd, &prompt, ctx);
    }
    run_pr_summary_headless(client, model, cwd, &prompt)
}

fn run_pr_summary_headless(
    client: &DetectedClient,
    model: &str,
    cwd: &Path,
    prompt: &str,
) -> Result<(ProbePrSummary, PathBuf)> {
    let mut last_err = None;
    for attempt in 1..=2 {
        if attempt > 1 {
            eprintln!("scrutiny probe: pr summary retry ({attempt}/2)…");
        }
        let out = run_headless(
            client,
            model,
            cwd,
            prompt,
            HeadlessKind::Summary,
            "summary#1",
            crate::timeouts::probe_summary(),
        )?;
        match parse_pr_summary_raw(&client.client, &out.stdout) {
            Ok(summary) => {
                return finalize_pr_summary(
                    summary,
                    client,
                    model,
                    out.session_id,
                    out.request_id,
                    Some(out.wall_ms),
                );
            }
            Err(e) => {
                eprintln!("scrutiny probe: warn: pr summary parse: {e:#}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("pr summary agent failed")))
}

fn run_pr_summary_nonheadless(
    client: &DetectedClient,
    model: &str,
    cwd: &Path,
    prompt: &str,
    ctx: &ResolvedTerminal,
) -> Result<(ProbePrSummary, PathBuf)> {
    let mut last_err = None;
    for attempt in 1..=2 {
        if attempt > 1 {
            eprintln!("scrutiny probe: pr summary retry ({attempt}/2)…");
        }
        let file_path = artifact_path("review-pr-summary-out");
        let _ = fs::remove_file(&file_path);
        let full_prompt = format!("{prompt}{}", nonheadless_summary_suffix(&file_path));
        let sentinel = run_nonheadless(client, model, cwd, &full_prompt, "summary#1", ctx)?;
        let wall = crate::timeouts::probe_summary().max(crate::timeouts::nonheadless());
        let missing = wait_for_sentinels(&[sentinel], wall);
        if !missing.is_empty() {
            eprintln!(
                "scrutiny probe: warn: summary pane did not signal done within {}s",
                wall.as_secs()
            );
        }
        let raw = match fs::read_to_string(&file_path) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!(
                    "could not read summary file {}: {e}",
                    file_path.display()
                );
                eprintln!("scrutiny probe: warn: pr summary: {msg}");
                last_err = Some(anyhow::anyhow!(msg));
                continue;
            }
        };
        match parse_pr_summary_raw(&client.client, &raw) {
            Ok(summary) => {
                return finalize_pr_summary(summary, client, model, None, None, None);
            }
            Err(e) => {
                eprintln!("scrutiny probe: warn: pr summary parse: {e:#}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("pr summary agent failed")))
}

/// Convert anchored PR-summary concerns into consolidator input findings.
/// Unanchored concerns are skipped (display-only / triage promote path).
pub fn summary_concerns_as_findings(summary: &ProbePrSummary) -> Vec<AgentFinding> {
    summary
        .bad_points
        .iter()
        .filter(|c| c.is_actionable())
        .map(|c| {
            let path = c
                .path
                .as_ref()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .unwrap_or_default();
            let line = c.line.unwrap_or(0);
            let title = if let Some(t) = c.title.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty())
            {
                t.to_string()
            } else {
                let first = c.text.lines().next().unwrap_or(c.text.as_str()).trim();
                if first.chars().count() <= 72 {
                    first.to_string()
                } else {
                    let truncated: String = first.chars().take(69).collect();
                    format!("{truncated}…")
                }
            };
            AgentFinding {
                path,
                line,
                start_line: None,
                severity: normalize_severity(c.severity.as_deref().unwrap_or("suggestion")),
                title,
                explanation: c.text.clone(),
                proposed_fix: c.proposed_fix.clone().unwrap_or_default(),
                fix_options: Vec::new(),
                source_role: "summary".into(),
            }
        })
        .collect()
}

/// Collate agent results (+ optional extras such as PR-summary concerns) into a
/// deduped `ReviewReport` and write it.
///
/// After a cheap Rust dedupe pass, an LLM consolidation agent semantically
/// merges what the heuristic missed (keeps higher severity).
pub fn collate_review_report(
    agents: Vec<AgentRunResult>,
    spawn_mode: &str,
    client: &DetectedClient,
    model: &str,
    pack_path: &Path,
    cwd: &Path,
    extra: Vec<AgentFinding>,
) -> Result<(ReviewReport, PathBuf)> {
    let raw_count: u32 = agents.iter().map(|a| a.findings.len() as u32).sum::<u32>()
        + extra.len() as u32;
    let mut all: Vec<AgentFinding> = agents.iter().flat_map(|a| a.findings.clone()).collect();
    all.extend(extra);
    let deduped = dedupe_findings(&mut all);
    let findings = consolidate_findings(client, model, pack_path, cwd, deduped);
    let usage_total = {
        let mut t = TokenUsage::default();
        let mut any = false;
        for a in &agents {
            if let Some(u) = &a.usage {
                t.add_assign(u);
                any = true;
            }
        }
        any.then_some(t)
    };
    let report = ReviewReport {
        version: 1,
        spawn_mode: spawn_mode.to_string(),
        model: model.to_string(),
        findings,
        agents,
        deduped_from: raw_count,
        usage_total,
    };
    let out = temp_artifact_path(&client.client, "review", "report");
    write_json_pretty(&out, &report)?;
    Ok((report, out))
}

/// Spawn one headless consolidation agent to semantically dedupe isolated
/// findings. Falls back to the input list on any failure (empty/parse/auth) or
/// when `SCRUTINY_NO_CONSOLIDATE` is set. No-op for <=1 finding.
fn consolidate_findings(
    client: &DetectedClient,
    model: &str,
    pack_path: &Path,
    cwd: &Path,
    findings: Vec<AgentFinding>,
) -> Vec<AgentFinding> {
    if findings.len() <= 1 || std::env::var_os("SCRUTINY_NO_CONSOLIDATE").is_some() {
        return findings;
    }
    let findings_json = match serde_json::to_string(&serde_json::json!({ "findings": &findings })) {
        Ok(j) => j,
        Err(e) => {
            eprintln!(
                "scrutiny: consolidate: serialize failed ({e}) — using Rust-deduped findings"
            );
            return findings;
        }
    };
    let prompt = build_consolidation_prompt(&findings_json, pack_path);
    eprintln!("scrutiny: consolidating {} findings…", findings.len());
    match run_headless(
        client,
        model,
        cwd,
        &prompt,
        HeadlessKind::Consolidate,
        "consolidator",
        crate::timeouts::probe_consolidate(),
    ) {
        Ok(out) => {
            if let Some(err) = claude_error_message(&out.stdout) {
                eprintln!(
                    "scrutiny: consolidate: agent error ({err}) — using Rust-deduped findings"
                );
                return findings;
            }
            match parse_findings_json(&out.stdout, "consolidator") {
                Ok(f) if !f.is_empty() => {
                    eprintln!(
                        "scrutiny: consolidated {} → {} findings",
                        findings.len(),
                        f.len()
                    );
                    f
                }
                _ => {
                    eprintln!("scrutiny: consolidate: empty/unparseable output — using Rust-deduped findings");
                    findings
                }
            }
        }
        Err(e) => {
            eprintln!("scrutiny: consolidate: spawn failed ({e:#}) — using Rust-deduped findings");
            findings
        }
    }
}

/// Run isolated parallel specialists; collate + dedupe into ReviewReport.
pub fn build_pr_summary_prompt(pack_path: &Path) -> String {
    let header = crate::caveman::dialect(
        "Scrutiny PR summary specialist. ISOLATED. No subagents. No findings array.",
        "Scrutiny PR summary specialist. ISOLATED mode. No subagents. No findings array.",
    );
    format!(
        r#"{header}

Read the pack. Write a SHORT overview for a reviewer about to triage findings.

Tier 0: pack only — diffs, symbol slices, annex, outlined names, referenced_signatures.
Tier 1: pack lists `dropped_regions[].fetch_cmd` or `explore.allowed_paths` → MAY Read that path.
Tier 2: at most 6 extra Reads of head files already in pack/xref/imports. No writes.

Output JSON ONLY. No prose outside JSON.
{{
  "purpose": "one short sentence: what changed",
  "architecture": "one short sentence: how it is wired (modules / data flow)",
  "good_points": ["short strength"],
  "bad_points": [
    {{
      "text": "actionable risk (one clear sentence)",
      "path": "rel/path/from/pack",
      "line": 42,
      "severity": "suggestion",
      "title": "optional short title",
      "proposed_fix": "optional concrete fix"
    }}
  ],
  "review_limits": ["pack caveat only if any"],
  "diagram": "",
  "table": ""
}}

Style (mandatory):
- Plain, concise English. No flourish, metaphor, or Shakespearean wording.
- Prefer short words. Cut filler. Every sentence must earn its place.
- `purpose`: ONE sentence, ≤25 words.
- `architecture`: ONE sentence (two only if data flow needs it), ≤30 words.
- `good_points`: 0–3 bullets; each ≤12 words; concrete (module/pattern), not praise.
- `bad_points`: actionable code risks ONLY. Each MUST include `path` + `line` from pack
  unified diff / symbol slices (1-based new-file lines). Prefer `suggestion` or `warning`.
  0–5 items; empty if none.
- `review_limits`: pack/tool caveats only (truncation, unread regions). Empty if none.
  NEVER put pack limits in `bad_points`.
- `diagram` / `table`: EMPTY string by default. Fill ONLY when a visual is necessary to
  understand structure or a comparison that prose cannot make clear. Prefer nothing.
  diagram = mermaid or compact ascii; table = github-flavored markdown table.
- JSON keys exact. Do NOT emit a `findings` array. Anchored `bad_points` promote later.

## Pack
Pack: `{pack}`
Prefer pack.md sibling if present (same stem). Read entire pack.
"#,
        pack = pack_path.display()
    )
}

pub fn parse_pr_summary_json(raw: &str) -> Result<ProbePrSummary> {
    let payload = extract_json_payload(raw)?;
    let v: Value = serde_json::from_str(&payload).context("parse PR summary JSON")?;
    Ok(parse_pr_summary_value(&v))
}

fn summary_has_content(s: &ProbePrSummary) -> bool {
    !s.purpose.is_empty() || !s.architecture.is_empty()
}

fn parse_string_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn parse_concern_item(item: &Value) -> Option<SummaryConcern> {
    if let Some(s) = item.as_str() {
        let text = s.trim().to_string();
        if text.is_empty() {
            return None;
        }
        // Legacy plain-string concern — keep for display; not actionable.
        return Some(SummaryConcern {
            text,
            path: None,
            line: None,
            severity: None,
            title: None,
            proposed_fix: None,
        });
    }
    if !item.is_object() {
        return None;
    }
    let text = item
        .get("text")
        .or_else(|| item.get("concern"))
        .or_else(|| item.get("explanation"))
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            item.get("title")
                .and_then(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })?;
    let path = item
        .get("path")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let line = item
        .get("line")
        .and_then(|x| x.as_u64())
        .map(|n| n as u32)
        .filter(|&n| n > 0);
    let severity = item
        .get("severity")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let title = item
        .get("title")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let proposed_fix = item
        .get("proposed_fix")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(SummaryConcern {
        text,
        path,
        line,
        severity,
        title,
        proposed_fix,
    })
}

fn parse_bad_points(v: &Value) -> Vec<SummaryConcern> {
    v.get("bad_points")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(parse_concern_item).collect())
        .unwrap_or_default()
}

fn summary_from_object(v: &Value) -> ProbePrSummary {
    ProbePrSummary {
        purpose: v
            .get("purpose")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        architecture: v
            .get("architecture")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        good_points: parse_string_list(v, "good_points"),
        bad_points: parse_bad_points(v),
        review_limits: parse_string_list(v, "review_limits"),
        diagram: v
            .get("diagram")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        table: v
            .get("table")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
    }
}

/// Unwrap Claude/Cursor envelopes. Prefer the first non-empty summary among:
/// direct object → `structured_output` (object or JSON string) → `result` string.
/// Empty/`null` `structured_output` must NOT block a good `result` payload.
fn parse_pr_summary_value(v: &Value) -> ProbePrSummary {
    let direct = summary_from_object(v);
    if summary_has_content(&direct) {
        return direct;
    }

    if let Some(so) = v.get("structured_output") {
        if let Some(s) = so.as_str().filter(|s| !s.trim().is_empty()) {
            if let Ok(inner) = serde_json::from_str::<Value>(s) {
                let parsed = parse_pr_summary_value(&inner);
                if summary_has_content(&parsed) {
                    return parsed;
                }
            } else if let Ok(parsed) = parse_pr_summary_json(s) {
                if summary_has_content(&parsed) {
                    return parsed;
                }
            }
        } else if so.is_object() {
            let parsed = summary_from_object(so);
            if summary_has_content(&parsed) {
                return parsed;
            }
        }
        // null / {} / empty fields → fall through to `result`
    }

    if let Some(r) = v
        .get("result")
        .and_then(|x| x.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        if let Ok(parsed) = parse_pr_summary_json(r) {
            if summary_has_content(&parsed) {
                return parsed;
            }
        }
    }

    direct
}

pub fn run_isolated_agents(
    client: &DetectedClient,
    plan: &ConfirmedPlan,
    pack_path: &Path,
    cwd: &Path,
    term: Option<&ResolvedTerminal>,
) -> Result<Vec<AgentRunResult>> {
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

    // Non-headless: each agent writes findings JSON to a per-agent file.
    if let Some(ctx) = term {
        let wall = crate::timeouts::nonheadless();
        // (sentinel_path, findings_path, role, index, paths)
        let mut entries: Vec<(PathBuf, PathBuf, String, u32, Vec<String>)> = Vec::new();
        for (role, index, paths) in &jobs {
            let label = format!("{role}#{index}");
            let findings_path =
                artifact_path(&format!("review-agent-findings-{role}-{index}"));
            let prompt = build_isolated_prompt(role, pack_path, paths, plan)
                + &nonheadless_findings_suffix(&findings_path);
            let sentinel = run_nonheadless(client, &plan.model, cwd, &prompt, &label, ctx)?;
            entries.push((sentinel, findings_path, role.clone(), *index, paths.clone()));
        }
        let sentinel_paths: Vec<PathBuf> =
            entries.iter().map(|(s, _, _, _, _)| s.clone()).collect();
        let missing = wait_for_sentinels(&sentinel_paths, wall);
        if !missing.is_empty() {
            eprintln!(
                "scrutiny: {} isolated agent window(s) did not signal done within {}s — collecting partial findings",
                missing.len(),
                crate::timeouts::get().nonheadless
            );
        }
        // Do not force-close here — summary#1 may still be running in parallel.
        // review_cmd closes leftovers after the summary agent joins.
        let mut agents: Vec<AgentRunResult> = Vec::new();
        for (_sentinel, findings_path, role, index, paths) in entries {
            let (findings, ok, stderr) = match fs::read_to_string(&findings_path) {
                Ok(raw) => {
                    let f = parse_findings_json(&raw, &role).unwrap_or_default();
                    let ok = !f.is_empty();
                    (f, ok, String::new())
                }
                Err(e) => (
                    Vec::new(),
                    false,
                    format!("could not read findings file: {e}"),
                ),
            };
            if !ok {
                eprintln!("scrutiny: agent {role}#{index} non-headless: no findings or read error");
            }
            agents.push(AgentRunResult {
                role,
                index,
                paths,
                findings,
                ok,
                stderr,
                usage: None,
                request_id: None,
                session_id: None,
                wall_ms: None,
            });
        }
        return Ok(agents);
    }

    let wall = crate::timeouts::probe_isolated();
    let pending: std::sync::Arc<std::sync::Mutex<Vec<String>>> = std::sync::Arc::new(
        std::sync::Mutex::new(jobs.iter().map(|(r, i, _)| format!("{r}#{i}")).collect()),
    );

    eprintln!(
        "scrutiny: spawning {} isolated agents (wall {}m): {}",
        jobs.len(),
        wall.as_secs() / 60,
        pending.lock().map(|p| p.join(", ")).unwrap_or_default()
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
                        usage: out.usage,
                        request_id: out.request_id,
                        session_id: out.session_id,
                        wall_ms: Some(out.wall_ms),
                    }
                }
                Err(e) => AgentRunResult {
                    role,
                    index,
                    paths,
                    findings: Vec::new(),
                    ok: false,
                    stderr: format!("{e:#}"),
                    usage: None,
                    request_id: None,
                    session_id: None,
                    wall_ms: None,
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
        match rx.recv_timeout(Duration::from_secs(progress_secs()).min(wait)) {
            Ok(r) => {
                let n = r.findings.len();
                eprintln!(
                    "scrutiny: collected {}#{} findings={n} ok={}",
                    r.role, r.index, r.ok
                );
                agents.push(r);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_progress.elapsed() >= Duration::from_secs(progress_secs()) {
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
                    eprintln!(
                        "scrutiny: batch wall exceeded — using {} finished agents",
                        agents.len()
                    );
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
            "isolated review: every agent failed. First error: {sample}\n{}",
            isolated_all_failed_hint(&client.client)
        );
    }

    Ok(agents)
}

/// Spawn isolated agents, then collate (+ optional extras) into a report.
pub fn run_isolated_review(
    client: &DetectedClient,
    plan: &ConfirmedPlan,
    pack_path: &Path,
    cwd: &Path,
    term: Option<&ResolvedTerminal>,
) -> Result<(ReviewReport, PathBuf)> {
    let agents = run_isolated_agents(client, plan, pack_path, cwd, term)?;
    collate_review_report(
        agents,
        "isolated",
        client,
        &plan.model,
        pack_path,
        cwd,
        Vec::new(),
    )
}

pub fn run_team_agents(
    client: &DetectedClient,
    plan: &ConfirmedPlan,
    pack_path: &Path,
    cwd: &Path,
    term: Option<&ResolvedTerminal>,
) -> Result<Vec<AgentRunResult>> {
    let prompt_base = build_team_lead_prompt(pack_path, plan);

    if let Some(ctx) = term {
        let findings_path = artifact_path("review-lead-findings");
        let prompt = prompt_base + &nonheadless_findings_suffix(&findings_path);
        let sentinel = run_nonheadless(client, &plan.model, cwd, &prompt, "lead#1", ctx)?;
        let missing = wait_for_sentinels(&[sentinel], crate::timeouts::nonheadless());
        if !missing.is_empty() {
            eprintln!(
                "scrutiny: team lead window did not signal done within {}s — collecting partial findings",
                crate::timeouts::get().nonheadless
            );
        }
        // Do not force-close here — summary#1 may still be running in parallel.
        let (findings, ok, stderr) = match fs::read_to_string(&findings_path) {
            Ok(raw) => {
                let f = parse_findings_json(&raw, "lead").unwrap_or_default();
                let ok = !f.is_empty();
                (f, ok, String::new())
            }
            Err(e) => (
                Vec::new(),
                false,
                format!("could not read findings file: {e}"),
            ),
        };
        let agent = AgentRunResult {
            role: "lead".into(),
            index: 1,
            paths: Vec::new(),
            findings,
            ok,
            stderr,
            usage: None,
            request_id: None,
            session_id: None,
            wall_ms: None,
        };
        return Ok(vec![agent]);
    }

    let wall = crate::timeouts::probe_team();
    let out = run_headless(
        client,
        &plan.model,
        cwd,
        &prompt_base,
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
    let ok = out.code == 0 || !findings.is_empty();
    let agent = AgentRunResult {
        role: "lead".into(),
        index: 1,
        paths: Vec::new(),
        findings,
        ok,
        stderr: out.stderr,
        usage: out.usage.clone(),
        request_id: out.request_id.clone(),
        session_id: out.session_id.clone(),
        wall_ms: Some(out.wall_ms),
    };
    Ok(vec![agent])
}

/// Spawn team lead, then collate (+ optional extras) into a report.
pub fn run_team_review(
    client: &DetectedClient,
    plan: &ConfirmedPlan,
    pack_path: &Path,
    cwd: &Path,
    term: Option<&ResolvedTerminal>,
) -> Result<(ReviewReport, PathBuf)> {
    let agents = run_team_agents(client, plan, pack_path, cwd, term)?;
    collate_review_report(agents, "team", client, &plan.model, pack_path, cwd, Vec::new())
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
/// Applies the same [`inject_overrides`] path as real spawns (caveman + [prompts]).
pub fn run_agent_prompt(input: AgentPromptInput) -> Result<String> {
    let plan = if let Some(p) = &input.plan_path {
        let text = fs::read_to_string(p).with_context(|| format!("read plan {}", p.display()))?;
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
    let label = if role == "lead" || role == "team" || role == "team_lead" {
        "lead#1"
    } else {
        role.as_str()
    };
    Ok(inject_overrides(label, &text))
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
    fn label_to_agent_type() {
        assert_eq!(agent_type_from_label("reviewer#3"), "reviewer");
        assert_eq!(agent_type_from_label("parley-member#1"), "parley_member");
        assert_eq!(agent_type_from_label("forge-implement"), "forge_implement");
        assert_eq!(agent_type_from_label("lead#1"), "lead");
    }

    #[test]
    fn caveman_always_injected() {
        let _g = crate::caveman::tests::lock_for_test();
        crate::caveman::store_caveman_enabled(true);
        std::env::remove_var("SCRUTINY_NO_CAVEMAN");
        std::env::remove_var("SCRUTINY_BENCH_SKILL_PREAMBLE");
        let out = inject_overrides("parley-member#1", "Fix the thing.");
        assert!(
            out.contains("# STYLE (mandatory) — caveman ultra"),
            "expected embedded ultra preamble"
        );
        assert!(out.ends_with("Fix the thing."));
    }

    #[test]
    fn summary_skips_caveman_preamble() {
        let _g = crate::caveman::tests::lock_for_test();
        crate::caveman::store_caveman_enabled(true);
        std::env::remove_var("SCRUTINY_NO_CAVEMAN");
        std::env::remove_var("SCRUTINY_BENCH_SKILL_PREAMBLE");
        let out = inject_overrides("summary#1", "JSON ONLY.");
        assert!(
            !out.contains("# STYLE (mandatory) — caveman ultra"),
            "summary must not get caveman preamble"
        );
        assert_eq!(out, "JSON ONLY.");
    }

    #[test]
    fn caveman_skipped_when_env_set() {
        let _g = crate::caveman::tests::lock_for_test();
        crate::caveman::store_caveman_enabled(true);
        std::env::set_var("SCRUTINY_NO_CAVEMAN", "1");
        std::env::remove_var("SCRUTINY_BENCH_SKILL_PREAMBLE");
        let out = inject_overrides("reviewer#1", "Review me.");
        std::env::remove_var("SCRUTINY_NO_CAVEMAN");
        assert_eq!(out, "Review me.");
    }

    #[test]
    fn dialect_isolated_prompt_smoke() {
        let _g = crate::caveman::tests::lock_for_test();
        crate::caveman::store_caveman_enabled(true);
        std::env::remove_var("SCRUTINY_NO_CAVEMAN");
        let plan = minimal_plan_for_prompt(Path::new("/tmp/pack.json"));
        let cv = build_isolated_prompt("reviewer", Path::new("/tmp/pack.json"), &[], &plan);
        assert!(cv.contains("Context policy (save tokens)"), "caveman body");
        crate::caveman::store_caveman_enabled(false);
        let en = build_isolated_prompt("reviewer", Path::new("/tmp/pack.json"), &[], &plan);
        crate::caveman::store_caveman_enabled(true);
        assert!(en.contains("Context policy (graduated"), "english body");
    }

    #[test]
    fn parse_claude_usage_envelope() {
        let raw = r#"{
          "type":"result","session_id":"s1","request_id":"r1",
          "usage":{"input_tokens":10,"output_tokens":5,
            "cache_creation_input_tokens":1,"cache_read_input_tokens":100}
        }"#;
        let (u, rid, sid) = parse_claude_usage(raw);
        let u = u.expect("usage");
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 5);
        assert_eq!(u.cache_creation_input_tokens, 1);
        assert_eq!(u.cache_read_input_tokens, 100);
        assert_eq!(rid.as_deref(), Some("r1"));
        assert_eq!(sid.as_deref(), Some("s1"));
    }

    #[test]
    fn auto_support_by_model() {
        for m in [
            "opus",
            "sonnet",
            "fable",
            "claude-opus-4-8",
            "claude-sonnet-4-6",
        ] {
            assert!(model_supports_auto(m), "{m} should support auto");
        }
        for m in [
            "haiku",
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-5",
            "claude-opus-4-5",
            "claude-3-5-sonnet",
        ] {
            assert!(!model_supports_auto(m), "{m} should NOT support auto");
        }
    }

    #[test]
    fn claude_error_from_stdout() {
        let raw =
            r#"{"type":"result","is_error":true,"result":"Not logged in · Please run /login"}"#;
        let msg = claude_error_message(raw).unwrap();
        assert!(msg.contains("Not logged in"));
    }

    #[test]
    fn claude_error_aborted_streaming() {
        let raw = r#"{
            "type":"result","is_error":true,"duration_api_ms":0,
            "terminal_reason":"aborted_streaming","subtype":"error_during_execution",
            "errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"]
        }"#;
        let msg = claude_error_message(raw).unwrap();
        assert!(msg.contains("aborted_streaming"), "{msg}");
        assert!(msg.contains("ede_diagnostic"), "{msg}");
        assert!(msg.contains("error_during_execution"), "{msg}");
    }

    #[test]
    fn no_stdout_early_kill_decision() {
        let wall = Duration::from_secs(600);
        assert!(!should_kill_for_no_stdout(
            false,
            Duration::from_secs(30),
            90,
            wall
        ));
        assert!(should_kill_for_no_stdout(
            false,
            Duration::from_secs(90),
            90,
            wall
        ));
        assert!(!should_kill_for_no_stdout(
            true,
            Duration::from_secs(200),
            90,
            wall
        ));
        assert!(!should_kill_for_no_stdout(
            false,
            Duration::from_secs(200),
            0,
            wall
        ));
        // Cap at wall when first-output > wall.
        assert!(should_kill_for_no_stdout(
            false,
            Duration::from_secs(10),
            90,
            Duration::from_secs(10)
        ));
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

    #[test]
    fn consolidation_prompt_has_severity_rule() {
        let raw = r#"{"findings":[{"path":"a.rs","line":1,"title":"t","severity":"warning"}]}"#;
        let p = build_consolidation_prompt(raw, Path::new("/tmp/pack.json"));
        assert!(p.contains("consolidator") || p.contains("Consolidator"));
        assert!(p.contains("higher") && p.contains("severity"));
        assert!(p.contains("critical > warning > suggestion"));
        assert!(p.contains("PR-summary") || p.contains("summary"));
        assert!(p.contains(r#""findings":["#));
        assert!(p.contains(raw));
    }

    #[test]
    fn summary_concerns_as_findings_skips_unanchored() {
        let summary = ProbePrSummary {
            purpose: "p".into(),
            architecture: "a".into(),
            good_points: vec![],
            bad_points: vec![
                SummaryConcern {
                    text: "anchored risk".into(),
                    path: Some("src/a.rs".into()),
                    line: Some(10),
                    severity: Some("warning".into()),
                    title: Some("Risk".into()),
                    proposed_fix: Some("fix it".into()),
                },
                SummaryConcern {
                    text: "no anchor".into(),
                    path: None,
                    line: None,
                    severity: None,
                    title: None,
                    proposed_fix: None,
                },
            ],
            review_limits: vec![],
            diagram: String::new(),
            table: String::new(),
        };
        let out = summary_concerns_as_findings(&summary);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "src/a.rs");
        assert_eq!(out[0].line, 10);
        assert_eq!(out[0].source_role, "summary");
        assert_eq!(out[0].title, "Risk");
    }

    #[test]
    fn nonheadless_invoke_cursor_tui_flags() {
        let line = nonheadless_invoke_line(
            "cursor",
            Path::new("/bin/agent"),
            "auto",
            Path::new("/tmp/ws"),
            Path::new("/tmp/prompt.txt"),
        )
        .unwrap();
        assert!(line.contains("--trust"), "{line}");
        assert!(line.contains("--force"), "{line}");
        assert!(line.contains("--workspace '/tmp/ws'"), "{line}");
        assert!(line.contains("--model 'auto'"), "{line}");
        assert!(
            !line.contains(" -p ") && !line.contains("'-p'") && !line.contains(" --print"),
            "{line}"
        );
        assert!(!line.contains("--mode "), "{line}");
        assert!(line.contains("$(cat '/tmp/prompt.txt')"), "{line}");
    }

    #[test]
    fn nonheadless_invoke_claude_unchanged() {
        let line = nonheadless_invoke_line(
            "claude",
            Path::new("/bin/claude"),
            "sonnet",
            Path::new("/tmp/ws"),
            Path::new("/tmp/prompt.txt"),
        )
        .unwrap();
        assert!(line.contains("--permission-mode auto"), "{line}");
        assert!(line.contains("--model 'sonnet'"), "{line}");
        assert!(!line.contains("--trust"), "{line}");
        assert!(!line.contains("--force"), "{line}");
        assert!(!line.contains("--workspace"), "{line}");
    }

    #[test]
    fn nonheadless_invoke_codex_rejected() {
        let err = nonheadless_invoke_line(
            "codex",
            Path::new("/bin/codex"),
            "gpt-5.5-medium",
            Path::new("/tmp/ws"),
            Path::new("/tmp/p"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("claude and cursor only"), "{err}");
        assert!(err.contains("codex"), "{err}");
    }

    #[test]
    fn parse_pr_summary_json_extracts_fields() {
        let raw = r#"{
          "purpose":"Adds auth",
          "architecture":"Service layer",
          "good_points":["Clean split"],
          "bad_points":[{"text":"No tests","path":"src/auth.ts","line":10,"severity":"warning"}],
          "review_limits":["Pack truncated e2e spec"]
        }"#;
        let s = parse_pr_summary_json(raw).unwrap();
        assert_eq!(s.purpose, "Adds auth");
        assert_eq!(s.architecture, "Service layer");
        assert_eq!(s.good_points, vec!["Clean split"]);
        assert_eq!(s.bad_points.len(), 1);
        assert_eq!(s.bad_points[0].text, "No tests");
        assert_eq!(s.bad_points[0].path.as_deref(), Some("src/auth.ts"));
        assert_eq!(s.bad_points[0].line, Some(10));
        assert!(s.bad_points[0].is_actionable());
        assert_eq!(s.review_limits, vec!["Pack truncated e2e spec"]);
    }

    #[test]
    fn parse_pr_summary_accepts_legacy_string_bad_points() {
        let raw = r#"{"purpose":"Adds auth","architecture":"Service layer","good_points":["Clean split"],"bad_points":["No tests"]}"#;
        let s = parse_pr_summary_json(raw).unwrap();
        assert_eq!(s.bad_points.len(), 1);
        assert_eq!(s.bad_points[0].text, "No tests");
        assert!(!s.bad_points[0].is_actionable());
    }

    #[test]
    fn parse_pr_summary_json_unwraps_claude_structured_output() {
        let raw = r#"{
          "type":"result","subtype":"success","is_error":false,
          "result":"",
          "structured_output":{
            "purpose":"Seat guest flow refactor",
            "architecture":"Hook extracts table seating state",
            "good_points":["Clear split"],
            "bad_points":[{"text":"Missing tests","path":"tests/seat.ts","line":1}],
            "review_limits":[]
          }
        }"#;
        let s = parse_pr_summary_json(raw).unwrap();
        assert_eq!(s.purpose, "Seat guest flow refactor");
        assert_eq!(s.architecture, "Hook extracts table seating state");
        assert_eq!(s.good_points, vec!["Clear split"]);
        assert_eq!(s.bad_points[0].text, "Missing tests");
        assert!(s.bad_points[0].is_actionable());
    }

    #[test]
    fn parse_pr_summary_json_unwraps_result_string() {
        let raw = r#"{
          "type":"result",
          "result":"{\"purpose\":\"Via result\",\"architecture\":\"Nested\",\"good_points\":[],\"bad_points\":[],\"review_limits\":[]}"
        }"#;
        let s = parse_pr_summary_json(raw).unwrap();
        assert_eq!(s.purpose, "Via result");
        assert_eq!(s.architecture, "Nested");
    }

    #[test]
    fn parse_pr_summary_falls_through_null_structured_output() {
        // Regression: null/empty structured_output used to short-circuit and
        // skip a good `result` string → empty purpose → triage hid the summary.
        let raw = r#"{
          "type":"result","subtype":"success","is_error":false,
          "result":"{\"purpose\":\"From result\",\"architecture\":\"Layers\",\"good_points\":[\"A\"],\"bad_points\":[],\"review_limits\":[]}",
          "structured_output":null
        }"#;
        let s = parse_pr_summary_json(raw).unwrap();
        assert_eq!(s.purpose, "From result");
        assert_eq!(s.architecture, "Layers");
        assert_eq!(s.good_points, vec!["A"]);
    }

    #[test]
    fn parse_pr_summary_falls_through_empty_structured_output_object() {
        let raw = r#"{
          "result":"{\"purpose\":\"Recovered\",\"architecture\":\"Via result\",\"good_points\":[],\"bad_points\":[\"Risk\"],\"review_limits\":[]}",
          "structured_output":{"purpose":"","architecture":"","good_points":[],"bad_points":[],"review_limits":[]}
        }"#;
        let s = parse_pr_summary_json(raw).unwrap();
        assert_eq!(s.purpose, "Recovered");
        assert_eq!(s.architecture, "Via result");
        assert_eq!(s.bad_points.len(), 1);
        assert_eq!(s.bad_points[0].text, "Risk");
    }

    #[test]
    fn parse_pr_summary_structured_output_json_string() {
        let raw = r#"{
          "structured_output":"{\"purpose\":\"SO string\",\"architecture\":\"Nested SO\",\"good_points\":[],\"bad_points\":[],\"review_limits\":[]}"
        }"#;
        let s = parse_pr_summary_json(raw).unwrap();
        assert_eq!(s.purpose, "SO string");
        assert_eq!(s.architecture, "Nested SO");
    }

    #[test]
    fn cursor_headless_omits_model_auto() {
        let args = cursor_headless_flags("auto", Path::new("/tmp/ws"), HeadlessKind::Isolated);
        assert!(!args.iter().any(|a| a == "--model"), "{args:?}");
        assert!(!args.iter().any(|a| a == "auto"), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["--mode", "ask"]), "{args:?}");
        assert!(args.contains(&"-p".into()), "{args:?}");
        assert!(
            args.windows(2).any(|w| w == ["--workspace", "/tmp/ws"]),
            "{args:?}"
        );
    }

    #[test]
    fn cursor_headless_keeps_concrete_model() {
        let args =
            cursor_headless_flags("composer-2-fast", Path::new("/tmp/ws"), HeadlessKind::Forge);
        assert!(
            args.windows(2).any(|w| w == ["--model", "composer-2-fast"]),
            "{args:?}"
        );
        assert!(!args.iter().any(|a| a == "--mode"), "{args:?}");
    }

    #[test]
    fn first_output_kill_skipped_for_cursor_only() {
        assert!(applies_first_output_kill("claude"));
        assert!(applies_first_output_kill("codex"));
        assert!(!applies_first_output_kill("cursor"));
        let wall = Duration::from_secs(600);
        assert!(should_kill_for_no_stdout(
            false,
            Duration::from_secs(90),
            90,
            wall
        ));
    }

    #[test]
    fn isolated_fail_hint_is_client_specific() {
        let cursor = isolated_all_failed_hint("cursor");
        assert!(cursor.contains("agent login"), "{cursor}");
        assert!(cursor.contains("CURSOR_API_KEY"), "{cursor}");
        assert!(!cursor.contains("ANTHROPIC_API_KEY"), "{cursor}");
        let claude = isolated_all_failed_hint("claude");
        assert!(claude.contains("/login"), "{claude}");
        assert!(claude.contains("ANTHROPIC_API_KEY"), "{claude}");
    }
}
