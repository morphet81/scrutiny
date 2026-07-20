use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chrono::Utc;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{ensure_config, find_shipped_default, load_config, SuggestedForge};
use crate::forge::complexity::estimate_ticket_tier;
use crate::forge::tools::{require_acli, require_gh, require_glab};
use crate::score::Tier;
use crate::paths::{temp_artifact_path, write_json_pretty};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TicketSource {
    Jira,
    Github,
    Gitlab,
    Inline,
}

impl TicketSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Jira => "jira",
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Inline => "inline",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketComment {
    pub author: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketReport {
    pub version: u32,
    pub source: String,
    pub id: String,
    pub url: Option<String>,
    pub title: String,
    pub description: String,
    pub labels: Vec<String>,
    pub comments: Vec<TicketComment>,
    pub attachments_dir: Option<String>,
    pub figma_urls: Vec<String>,
    /// Local dir with fcli screenshots + XML (set by forge orchestrator).
    #[serde(default)]
    pub figma_dir: Option<String>,
    pub fields: Value,
    pub raw_path: Option<String>,
    pub fetched_at: String,
    pub suggested_forge: SuggestedForge,
}

#[derive(Debug, Clone)]
pub struct ForgeFetchInput {
    pub cwd: PathBuf,
    /// Raw argument: URL, issue key/number, or inline description.
    pub input: Option<String>,
    /// Force source detection.
    pub source: Option<String>,
    /// Treat input as inline description (no remote fetch).
    pub inline: bool,
    pub client: Option<String>,
    pub title: Option<String>,
}

pub fn run_forge_fetch(input: ForgeFetchInput) -> Result<(TicketReport, PathBuf)> {
    let shipped = find_shipped_default(&input.cwd);
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;
    let client = input
        .client
        .clone()
        .unwrap_or_else(|| cfg.default_client.clone());

    let (source, raw_input) = resolve_source(&input)?;
    let mut report = match source {
        TicketSource::Inline => fetch_inline(&raw_input, input.title.as_deref())?,
        TicketSource::Jira => fetch_jira(&input.cwd, &raw_input)?,
        TicketSource::Github => fetch_github(&input.cwd, &raw_input)?,
        TicketSource::Gitlab => fetch_gitlab(&input.cwd, &raw_input)?,
    };
    // figma_urls must be set before complexity estimation (design signal)
    report.figma_urls = extract_figma_urls(&report);
    let (tier, score, breakdown) = estimate_ticket_tier(
        &report.title,
        &report.description,
        &report.labels,
        report.comments.len(),
        report.figma_urls.len(),
        &report.fields,
        &cfg.forge.complexity,
    );
    let reason = breakdown.top_signals.join(", ");
    report.suggested_forge = cfg.suggested_forge(&client, tier, score, reason);

    let _ = crate::paths::init_artifact_ctx(
        &input.cwd,
        &crate::paths::session_name(None, Some(&report.id)),
    );
    let path = temp_artifact_path("forge", &slug_id(&report.id), "ticket");
    write_json_pretty(&path, &report)?;
    Ok((report, path))
}

fn slug_id(id: &str) -> String {
    if id.is_empty() {
        "ticket".into()
    } else {
        id.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }
}

fn resolve_source(input: &ForgeFetchInput) -> Result<(TicketSource, String)> {
    if input.inline {
        let body = input
            .input
            .clone()
            .filter(|s| !s.trim().is_empty())
            .context("--inline requires a description")?;
        return Ok((TicketSource::Inline, body));
    }

    let raw = match &input.input {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            // Branch-name Jira key fallback
            if let Some(key) = jira_key_from_branch(&input.cwd)? {
                return Ok((TicketSource::Jira, key));
            }
            bail!(
                "no ticket input. Pass a URL, issue id, or --inline <description>. \
                 Or run from a branch containing a Jira key (e.g. feat/PROJ-123)."
            );
        }
    };

    if let Some(forced) = &input.source {
        let src = match forced.to_ascii_lowercase().as_str() {
            "jira" => TicketSource::Jira,
            "github" | "gh" => TicketSource::Github,
            "gitlab" | "gl" => TicketSource::Gitlab,
            "inline" => TicketSource::Inline,
            other => bail!("unknown --source {other} (jira|github|gitlab|inline)"),
        };
        return Ok((src, raw));
    }

    Ok((detect_source(&raw), raw))
}

fn detect_source(raw: &str) -> TicketSource {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("atlassian.net")
        || lower.contains(".jira.com")
        || lower.contains("/browse/")
        || is_jira_key(raw)
    {
        return TicketSource::Jira;
    }
    if lower.contains("github.com") && lower.contains("/issues/") {
        return TicketSource::Github;
    }
    if lower.contains("gitlab.com") && lower.contains("/issues/") {
        return TicketSource::Gitlab;
    }
    // bare number → github in current repo
    if raw.chars().all(|c| c.is_ascii_digit()) {
        return TicketSource::Github;
    }
    if raw.starts_with('#') && raw[1..].chars().all(|c| c.is_ascii_digit()) {
        return TicketSource::Github;
    }
    if is_jira_key(raw) {
        return TicketSource::Jira;
    }
    TicketSource::Inline
}

fn is_jira_key(s: &str) -> bool {
    let s = s.trim();
    let mut parts = s.split('-');
    let Some(proj) = parts.next() else {
        return false;
    };
    let Some(num) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && proj.len() >= 2
        && proj
            .chars()
            .enumerate()
            .all(|(i, c)| if i == 0 { c.is_ascii_uppercase() } else { c.is_ascii_uppercase() || c.is_ascii_digit() })
        && !num.is_empty()
        && num.chars().all(|c| c.is_ascii_digit())
}

fn jira_key_from_branch(cwd: &Path) -> Result<Option<String>> {
    let out = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output();
    let Ok(out) = out else {
        return Ok(None);
    };
    if !out.status.success() {
        return Ok(None);
    }
    let branch = String::from_utf8_lossy(&out.stdout);
    Ok(extract_jira_key_from_text(&branch))
}

fn extract_jira_key_from_text(text: &str) -> Option<String> {
    // Scan for PROJ-123 style tokens
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_uppercase() {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_uppercase() || bytes[i].is_ascii_digit())
            {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'-' {
                i += 1;
                let num_start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i > num_start {
                    let candidate = &text[start..i];
                    if is_jira_key(candidate) {
                        return Some(candidate.to_string());
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

fn fetch_inline(body: &str, title: Option<&str>) -> Result<TicketReport> {
    let title = title
        .map(|s| s.to_string())
        .unwrap_or_else(|| first_line_title(body));
    Ok(TicketReport {
        version: 1,
        source: TicketSource::Inline.as_str().into(),
        id: "inline".into(),
        url: None,
        title,
        description: body.to_string(),
        labels: vec![],
        comments: vec![],
        attachments_dir: None,
        figma_urls: vec![],
        figma_dir: None,
        fields: Value::Object(Default::default()),
        raw_path: None,
        fetched_at: Utc::now().to_rfc3339(),
        suggested_forge: empty_suggested(),
    })
}

fn empty_suggested() -> SuggestedForge {
    SuggestedForge {
        client: String::new(),
        model: String::new(),
        tier: Tier::M,
        complexity_score: 0,
        complexity_reason: String::new(),
        available_models: vec![],
        approach: "tdd".into(),
        e2e: None,
        agents: 2,
        testers: 1,
        reviewers: 1,
        evangelists: 0,
        prompt_model: true,
        prompt_approach: true,
        prompt_e2e: true,
        prompt_agents: true,
        prompt_testers: true,
        prompt_reviewers: true,
        prompt_evangelists: true,
        enable_figma: true,
        enable_lore: true,
        enable_ticket_writeback: true,
        enable_po: true,
    }
}

fn first_line_title(body: &str) -> String {
    body.lines()
        .next()
        .unwrap_or("Inline task")
        .chars()
        .take(120)
        .collect()
}

fn fetch_jira(cwd: &Path, raw: &str) -> Result<TicketReport> {
    let key = jira_key_from_url_or_raw(raw)?;
    require_acli()?;
    let output = Command::new("acli")
        .args(["jira", "workitem", "view", &key, "--fields", "*all", "--json"])
        .current_dir(cwd)
        .output()
        .context("run acli jira workitem view")?;
    if !output.status.success() {
        bail!(
            "acli failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw_json: Value = serde_json::from_slice(&output.stdout).context("parse acli json")?;
    let raw_path = write_raw(cwd, &key, "jira", &raw_json)?;

    let title = raw_json
        .pointer("/fields/summary")
        .or_else(|| raw_json.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or(&key)
        .to_string();
    let description = extract_jira_description(&raw_json);
    let labels = raw_json
        .pointer("/fields/labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let comments = extract_jira_comments(&raw_json);
    let self_url = raw_json.get("self").and_then(|v| v.as_str());
    let url = jira_browse_url(&key, raw, self_url)
        .or_else(|| Some(format_jira_browse_url(raw, &key)));

    let attachments_dir = download_jira_attachments(cwd, &key, &raw_json)?;

    Ok(TicketReport {
        version: 1,
        source: TicketSource::Jira.as_str().into(),
        id: key,
        url,
        title,
        description,
        labels,
        comments,
        attachments_dir,
        figma_urls: vec![],
        figma_dir: None,
        fields: raw_json.get("fields").cloned().unwrap_or(Value::Null),
        raw_path: Some(raw_path.display().to_string()),
        fetched_at: Utc::now().to_rfc3339(),
        suggested_forge: empty_suggested(),
    })
}

fn jira_key_from_url_or_raw(raw: &str) -> Result<String> {
    if is_jira_key(raw) {
        return Ok(raw.trim().to_string());
    }
    if let Some(key) = extract_jira_key_from_text(raw) {
        return Ok(key);
    }
    // /browse/PROJ-123
    if let Some(idx) = raw.find("/browse/") {
        let rest = &raw[idx + "/browse/".len()..];
        let key = rest
            .split(|c: char| c == '/' || c == '?' || c == '#')
            .next()
            .unwrap_or("");
        if is_jira_key(key) {
            return Ok(key.to_string());
        }
    }
    bail!("could not extract Jira key from: {raw}");
}

/// Human, clickable Jira issue URL (`<host>/browse/KEY`) — never the REST
/// `self` endpoint. Derives the host from a browse-style raw input or from the
/// `self` link's host; `None` when no host is available.
fn jira_browse_url(key: &str, raw: &str, self_url: Option<&str>) -> Option<String> {
    if raw.contains("://") && raw.contains("/browse/") {
        return Some(format_jira_browse_url(raw, key));
    }
    let self_url = self_url?;
    let host_end = self_url.find("/rest/").unwrap_or(self_url.len());
    let host = self_url.get(..host_end)?;
    if host.contains("://") {
        Some(format!("{host}/browse/{key}"))
    } else {
        None
    }
}

fn format_jira_browse_url(raw: &str, key: &str) -> String {
    if raw.contains("://") {
        if let Some(idx) = raw.find("/browse/") {
            return format!("{}{}", &raw[..idx], format!("/browse/{key}"));
        }
        return raw.to_string();
    }
    key.to_string()
}

fn extract_jira_description(raw: &Value) -> String {
    let desc = raw
        .pointer("/fields/description")
        .or_else(|| raw.get("description"));
    match desc {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(_)) => {
            // ADF — flatten text nodes
            flatten_adf_text(desc.unwrap())
        }
        _ => String::new(),
    }
}

/// Render Atlassian Document Format (ADF) into GitHub-flavored Markdown so PR
/// bodies and comments keep their structure (headings, paragraphs, lists,
/// emphasis) instead of collapsing into one plain-text block.
fn flatten_adf_text(v: &Value) -> String {
    let mut out = String::new();
    render_adf(v, &mut out, "");
    // Collapse runs of 3+ newlines to a single blank line, then trim edges.
    let mut cleaned = String::with_capacity(out.len());
    let mut nl = 0usize;
    for ch in out.chars() {
        if ch == '\n' {
            nl += 1;
            if nl <= 2 {
                cleaned.push(ch);
            }
        } else {
            nl = 0;
            cleaned.push(ch);
        }
    }
    cleaned.trim().to_string()
}

/// Recursively render an ADF node. `prefix` is prepended to every line the node
/// produces (used for blockquotes); block nodes end with a blank line.
fn render_adf(v: &Value, out: &mut String, prefix: &str) {
    match v {
        Value::String(s) => out.push_str(s),
        Value::Array(arr) => {
            for item in arr {
                render_adf(item, out, prefix);
            }
        }
        Value::Object(map) => {
            let node_type = map.get("type").and_then(|x| x.as_str()).unwrap_or("");
            match node_type {
                "text" => {
                    let text = map.get("text").and_then(|x| x.as_str()).unwrap_or("");
                    out.push_str(&apply_marks(text, map.get("marks")));
                }
                "hardBreak" => out.push('\n'),
                "heading" => {
                    ensure_block_gap(out, prefix);
                    let level = v
                        .pointer("/attrs/level")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(2)
                        .clamp(1, 6) as usize;
                    out.push_str(prefix);
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                    render_children(v, out, prefix);
                    out.push_str("\n\n");
                }
                "paragraph" => {
                    ensure_block_gap(out, prefix);
                    out.push_str(prefix);
                    render_children(v, out, prefix);
                    out.push_str("\n\n");
                }
                "bulletList" | "orderedList" => {
                    ensure_block_gap(out, prefix);
                    let ordered = node_type == "orderedList";
                    if let Some(items) = map.get("content").and_then(|c| c.as_array()) {
                        for (i, item) in items.iter().enumerate() {
                            out.push_str(prefix);
                            if ordered {
                                out.push_str(&format!("{}. ", i + 1));
                            } else {
                                out.push_str("- ");
                            }
                            let mut inner = String::new();
                            render_children(item, &mut inner, prefix);
                            out.push_str(inner.trim());
                            out.push('\n');
                        }
                    }
                    out.push('\n');
                }
                "blockquote" => {
                    ensure_block_gap(out, prefix);
                    let quote_prefix = format!("{prefix}> ");
                    render_children(v, out, &quote_prefix);
                    out.push('\n');
                }
                "codeBlock" => {
                    ensure_block_gap(out, prefix);
                    let lang = v
                        .pointer("/attrs/language")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    out.push_str(&format!("```{lang}\n"));
                    render_children(v, out, "");
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("```\n\n");
                }
                "rule" => {
                    ensure_block_gap(out, prefix);
                    out.push_str("---\n\n");
                }
                _ => render_children(v, out, prefix),
            }
        }
        _ => {}
    }
}

/// Render an object's `content` children.
fn render_children(map: &Value, out: &mut String, prefix: &str) {
    if let Some(content) = map.get("content") {
        render_adf(content, out, prefix);
    }
}

/// Ensure the output is at the start of a fresh line with a blank line before a
/// new block (unless we're already at the very beginning).
fn ensure_block_gap(out: &mut String, prefix: &str) {
    if out.is_empty() || out == prefix {
        return;
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
}

/// Wrap `text` with the Markdown emphasis implied by an ADF `marks` array.
fn apply_marks(text: &str, marks: Option<&Value>) -> String {
    let Some(marks) = marks.and_then(|m| m.as_array()) else {
        return text.to_string();
    };
    let mut out = text.to_string();
    for mark in marks {
        match mark.get("type").and_then(|x| x.as_str()) {
            Some("strong") => out = format!("**{out}**"),
            Some("em") => out = format!("*{out}*"),
            Some("code") => out = format!("`{out}`"),
            Some("strike") => out = format!("~~{out}~~"),
            Some("link") => {
                if let Some(href) = mark.pointer("/attrs/href").and_then(|x| x.as_str()) {
                    out = format!("[{out}]({href})");
                }
            }
            _ => {}
        }
    }
    out
}

fn extract_jira_comments(raw: &Value) -> Vec<TicketComment> {
    let mut out = Vec::new();
    let comments = raw
        .pointer("/fields/comment/comments")
        .or_else(|| raw.pointer("/comment/comments"))
        .and_then(|v| v.as_array());
    let Some(arr) = comments else {
        return out;
    };
    for c in arr {
        let author = c
            .pointer("/author/displayName")
            .or_else(|| c.pointer("/author/emailAddress"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let body = match c.get("body") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => flatten_adf_text(other),
            None => String::new(),
        };
        out.push(TicketComment { author, body });
    }
    out
}

fn download_jira_attachments(cwd: &Path, key: &str, raw: &Value) -> Result<Option<String>> {
    let attachments = raw
        .pointer("/fields/attachment")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if attachments.is_empty() {
        return Ok(None);
    }
    // Prefer active forge session dir
    let root = crate::paths::init_artifact_ctx(
        cwd,
        &crate::paths::session_name(None, Some(key)),
    )?;
    let dir = root.join("attachments");
    fs::create_dir_all(&dir).context("create attachments dir")?;

    let profile = acli_jira_profile();
    let auth = resolve_jira_download_auth(cwd, profile.as_ref());
    let Some(auth) = auth else {
        eprintln!(
            "scrutiny forge: skip downloading {} attachment(s) for {key} \
             (no OAuth keychain token / API token; metadata stays in ticket.json)",
            attachments.len()
        );
        return Ok(Some(dir.display().to_string()));
    };

    let cloud_id = profile.as_ref().map(|p| p.cloud_id.as_str());
    let site = profile.as_ref().map(|p| p.site.as_str());

    eprintln!(
        "scrutiny forge: downloading {} attachment(s)…",
        attachments.len()
    );
    for att in attachments {
        let filename = att
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("attachment.bin");
        let Some(url) = attachment_download_url(cloud_id, site, &att, &auth) else {
            eprintln!("scrutiny forge: attachment missing id/url: {filename}");
            continue;
        };
        let dest = dir.join(filename);
        // Timeouts: bad auth + `-L` used to hang forever on login redirects.
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sS",
            "-L",
            "--connect-timeout",
            "10",
            "--max-time",
            "60",
            "-f",
            "-o",
            &dest.display().to_string(),
        ]);
        match &auth {
            JiraDownloadAuth::Bearer(token) => {
                cmd.args(["-H", &format!("Authorization: Bearer {token}")]);
            }
            JiraDownloadAuth::Basic { email, token } => {
                cmd.args(["-u", &format!("{email}:{token}")]);
            }
        }
        cmd.arg(&url);
        let status = cmd.status();
        if !matches!(status, Ok(s) if s.success()) {
            let _ = fs::remove_file(&dest);
            eprintln!("scrutiny forge: attachment download failed: {filename}");
        }
    }
    Ok(Some(dir.display().to_string()))
}

#[derive(Debug, Clone)]
enum JiraDownloadAuth {
    Bearer(String),
    Basic { email: String, token: String },
}

#[derive(Debug, Clone)]
struct AcliJiraProfile {
    cloud_id: String,
    account_id: String,
    site: String,
    email: String,
}

/// Env bearer → env API token (Basic) → macOS acli keychain OAuth → validated `acli auth token`.
fn resolve_jira_download_auth(cwd: &Path, profile: Option<&AcliJiraProfile>) -> Option<JiraDownloadAuth> {
    for key in ["SCRUTINY_JIRA_BEARER", "ATLASSIAN_ACCESS_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            let t = v.trim().to_string();
            if looks_like_bearer_token(&t) {
                return Some(JiraDownloadAuth::Bearer(t));
            }
        }
    }

    let api_token = ["ATLASSIAN_API_TOKEN", "JIRA_API_TOKEN"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(token) = api_token {
        let email = std::env::var("ATLASSIAN_EMAIL")
            .or_else(|_| std::env::var("JIRA_EMAIL"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| profile.map(|p| p.email.clone()).filter(|s| !s.is_empty()))
            .or_else(|| parse_acli_auth_status_email(cwd));
        if let Some(email) = email {
            return Some(JiraDownloadAuth::Basic { email, token });
        }
    }

    if let Some(p) = profile {
        if let Some(tok) = read_acli_oauth_access_token(p) {
            return Some(JiraDownloadAuth::Bearer(tok));
        }
    }

    // Legacy / future: only if acli ever ships a real `auth token`.
    let output = Command::new("acli")
        .args(["auth", "token"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if looks_like_bearer_token(&token) {
            return Some(JiraDownloadAuth::Bearer(token));
        }
    }
    None
}

fn attachment_download_url(
    cloud_id: Option<&str>,
    site: Option<&str>,
    att: &Value,
    auth: &JiraDownloadAuth,
) -> Option<String> {
    let id = attachment_id(att)?;
    match auth {
        JiraDownloadAuth::Bearer(_) => {
            let cid = cloud_id?;
            Some(format!(
                "https://api.atlassian.com/ex/jira/{cid}/rest/api/3/attachment/content/{id}"
            ))
        }
        JiraDownloadAuth::Basic { .. } => {
            if let Some(site) = site.filter(|s| !s.is_empty()) {
                let host = site.trim_start_matches("https://").trim_start_matches("http://");
                return Some(format!(
                    "https://{host}/rest/api/3/attachment/content/{id}"
                ));
            }
            // Last resort: content URL from JSON (may be unreachable off-VPN).
            att.get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }
    }
}

fn attachment_id(att: &Value) -> Option<String> {
    if let Some(s) = att.get("id").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    att.get("id")
        .and_then(|v| v.as_u64())
        .map(|n| n.to_string())
}

fn acli_jira_profile() -> Option<AcliJiraProfile> {
    let home = dirs_home()?;
    let path = home.join(".config/acli/jira_config.yaml");
    let text = fs::read_to_string(path).ok()?;
    parse_acli_jira_profile_yaml(&text)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Minimal parse of acli's jira_config.yaml (no YAML crate).
fn parse_acli_jira_profile_yaml(text: &str) -> Option<AcliJiraProfile> {
    let mut current_profile = None::<String>;
    let mut cloud_id = None::<String>;
    let mut account_id = None::<String>;
    let mut site = None::<String>;
    let mut email = None::<String>;
    for line in text.lines() {
        let t = line.trim().trim_start_matches('-').trim();
        if let Some(rest) = t.strip_prefix("current_profile:") {
            current_profile = Some(rest.trim().trim_matches('"').to_string());
        } else if let Some(rest) = t.strip_prefix("cloud_id:") {
            cloud_id = Some(rest.trim().trim_matches('"').to_string());
        } else if let Some(rest) = t.strip_prefix("account_id:") {
            account_id = Some(rest.trim().trim_matches('"').to_string());
        } else if let Some(rest) = t.strip_prefix("site:") {
            site = Some(rest.trim().trim_matches('"').to_string());
        } else if let Some(rest) = t.strip_prefix("email:") {
            email = Some(rest.trim().trim_matches('"').to_string());
        }
    }
    // current_profile is `{cloud_id}:{account_id}` — prefer explicit fields, else split UUID.
    if cloud_id.is_none() || account_id.is_none() {
        if let Some(cp) = &current_profile {
            if let Some((cid, aid)) = split_cloud_and_account(cp) {
                cloud_id = cloud_id.or(Some(cid));
                account_id = account_id.or(Some(aid));
            }
        }
    }
    Some(AcliJiraProfile {
        cloud_id: cloud_id.filter(|s| !s.is_empty())?,
        account_id: account_id.filter(|s| !s.is_empty())?,
        site: site.unwrap_or_default(),
        email: email.unwrap_or_default(),
    })
}

fn split_cloud_and_account(profile: &str) -> Option<(String, String)> {
    // UUID is 36 chars with dashes; rest after first ':' following UUID is account_id.
    let bytes = profile.as_bytes();
    if bytes.len() < 38 || bytes.get(36) != Some(&b':') {
        return None;
    }
    let cid = &profile[..36];
    if !cid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return None;
    }
    Some((cid.to_string(), profile[37..].to_string()))
}

fn parse_acli_auth_status_email(cwd: &Path) -> Option<String> {
    let output = Command::new("acli")
        .args(["auth", "status"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Email:") {
            let email = rest.trim();
            if !email.is_empty() {
                return Some(email.to_string());
            }
        }
    }
    None
}

/// Read renewing OAuth access_token from acli's go-keyring blob (macOS Keychain).
fn read_acli_oauth_access_token(profile: &AcliJiraProfile) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let acct = format!("jira:{}:{}", profile.cloud_id, profile.account_id);
        let output = Command::new("security")
            .args(["find-generic-password", "-s", "acli", "-a", &acct, "-w"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return decode_go_keyring_access_token(&raw);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = profile;
        None
    }
}

/// Decode `go-keyring-base64:` + gzip + JSON `{ access_token, ... }`.
fn decode_go_keyring_access_token(raw: &str) -> Option<String> {
    const PREFIX: &str = "go-keyring-base64:";
    let b64 = raw.strip_prefix(PREFIX)?.trim();
    let compressed = B64.decode(b64.as_bytes()).ok()?;
    let mut decoder = GzDecoder::new(&compressed[..]);
    let mut json_bytes = Vec::new();
    decoder.read_to_end(&mut json_bytes).ok()?;
    let v: Value = serde_json::from_slice(&json_bytes).ok()?;
    let token = v.get("access_token")?.as_str()?.to_string();
    if looks_like_bearer_token(&token) {
        Some(token)
    } else {
        None
    }
}

/// Reject acli help dumps / multi-line garbage that used to be passed as Bearer.
fn looks_like_bearer_token(s: &str) -> bool {
    let s = s.trim();
    // JWTs from Atlassian OAuth are long; keep headroom.
    if s.len() < 20 || s.len() > 16_384 {
        return false;
    }
    if s.contains('\n') || s.contains('\r') || s.contains(char::is_whitespace) {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if lower.contains("usage:") || lower.contains("authenticate to use") {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '/' | '='))
}

fn fetch_github(cwd: &Path, raw: &str) -> Result<TicketReport> {
    require_gh()?;
    let (repo, number) = parse_github_ref(cwd, raw)?;
    let mut args = vec![
        "issue".into(),
        "view".into(),
        number.clone(),
        "--json".into(),
        "number,title,body,labels,assignees,milestone,state,comments,url".into(),
    ];
    if let Some(r) = &repo {
        args.push("--repo".into());
        args.push(r.clone());
    }
    let output = Command::new("gh")
        .args(&args)
        .current_dir(cwd)
        .output()
        .context("run gh issue view")?;
    if !output.status.success() {
        bail!(
            "gh failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw_json: Value = serde_json::from_slice(&output.stdout).context("parse gh json")?;
    let id = format!(
        "#{}",
        raw_json
            .get("number")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| number.parse().unwrap_or(0))
    );
    let raw_path = write_raw(cwd, &id.replace('#', ""), "github", &raw_json)?;

    let title = raw_json
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = raw_json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let labels = raw_json
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    l.get("name")
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    let comments = raw_json
        .get("comments")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|c| TicketComment {
                    author: c
                        .pointer("/author/login")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    body: c
                        .get("body")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let url = raw_json
        .get("url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(TicketReport {
        version: 1,
        source: TicketSource::Github.as_str().into(),
        id,
        url,
        title,
        description,
        labels,
        comments,
        attachments_dir: None,
        figma_urls: vec![],
        figma_dir: None,
        fields: raw_json,
        raw_path: Some(raw_path.display().to_string()),
        fetched_at: Utc::now().to_rfc3339(),
        suggested_forge: empty_suggested(),
    })
}

fn parse_github_ref(cwd: &Path, raw: &str) -> Result<(Option<String>, String)> {
    let raw = raw.trim().trim_start_matches('#');
    // https://github.com/owner/repo/issues/42
    if let Some(rest) = raw
        .strip_prefix("https://github.com/")
        .or_else(|| raw.strip_prefix("http://github.com/"))
    {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 4 && parts[2] == "issues" {
            return Ok((Some(format!("{}/{}", parts[0], parts[1])), parts[3].to_string()));
        }
    }
    // owner/repo#42
    if let Some((repo, num)) = raw.split_once('#') {
        if repo.contains('/') && num.chars().all(|c| c.is_ascii_digit()) {
            return Ok((Some(repo.to_string()), num.to_string()));
        }
    }
    if raw.chars().all(|c| c.is_ascii_digit()) {
        // current repo
        let _ = cwd;
        return Ok((None, raw.to_string()));
    }
    bail!("could not parse GitHub issue ref: {raw}");
}

fn fetch_gitlab(cwd: &Path, raw: &str) -> Result<TicketReport> {
    require_glab()?;
    let (project, iid) = parse_gitlab_ref(raw)?;
    let mut cmd = Command::new("glab");
    cmd.args(["issue", "view", &iid, "-F", "json"]);
    if let Some(p) = &project {
        cmd.args(["--repo", p]);
    }
    let output = cmd.current_dir(cwd).output().context("run glab issue view")?;
    if !output.status.success() {
        bail!(
            "glab failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw_json: Value = serde_json::from_slice(&output.stdout).context("parse glab json")?;
    let id = format!(
        "#{}",
        raw_json
            .get("iid")
            .or_else(|| raw_json.get("id"))
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| iid.parse().unwrap_or(0))
    );
    let raw_path = write_raw(cwd, &id.replace('#', ""), "gitlab", &raw_json)?;

    let title = raw_json
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = raw_json
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let labels = raw_json
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let url = raw_json
        .get("web_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(TicketReport {
        version: 1,
        source: TicketSource::Gitlab.as_str().into(),
        id,
        url,
        title,
        description,
        labels,
        comments: vec![],
        attachments_dir: None,
        figma_urls: vec![],
        figma_dir: None,
        fields: raw_json,
        raw_path: Some(raw_path.display().to_string()),
        fetched_at: Utc::now().to_rfc3339(),
        suggested_forge: empty_suggested(),
    })
}

fn parse_gitlab_ref(raw: &str) -> Result<(Option<String>, String)> {
    let raw = raw.trim();
    // https://gitlab.com/group/proj/-/issues/42
    if let Some(idx) = raw.find("/-/issues/") {
        let before = &raw[..idx];
        let after = &raw[idx + "/-/issues/".len()..];
        let iid = after
            .split(|c: char| c == '/' || c == '?' || c == '#')
            .next()
            .unwrap_or("")
            .to_string();
        let project = before
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("gitlab.com/")
            .to_string();
        if !iid.is_empty() {
            return Ok((Some(project), iid));
        }
    }
    if raw.chars().all(|c| c.is_ascii_digit()) {
        return Ok((None, raw.to_string()));
    }
    bail!("could not parse GitLab issue ref: {raw}");
}

fn write_raw(cwd: &Path, id: &str, kind: &str, value: &Value) -> Result<PathBuf> {
    let repo = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo");
    let path = temp_artifact_path(repo, id, &format!("raw-{kind}"));
    write_json_pretty(&path, value)?;
    Ok(path)
}

fn extract_figma_urls(report: &TicketReport) -> Vec<String> {
    let mut urls = Vec::new();
    let mut scan = |text: &str| {
        for token in text.split_whitespace() {
            let t = token.trim_matches(|c: char| {
                c == '(' || c == ')' || c == '[' || c == ']' || c == ',' || c == '"' || c == '\''
            });
            if t.contains("figma.com/") {
                if !urls.iter().any(|u| u == t) {
                    urls.push(t.to_string());
                }
            }
        }
    };
    scan(&report.description);
    for c in &report.comments {
        scan(&c.body);
    }
    scan_value_for_figma(&report.fields, &mut scan);
    urls
}

fn scan_value_for_figma(v: &Value, scan: &mut dyn FnMut(&str)) {
    match v {
        Value::String(s) => scan(s),
        Value::Array(a) => {
            for x in a {
                scan_value_for_figma(x, scan);
            }
        }
        Value::Object(m) => {
            for x in m.values() {
                scan_value_for_figma(x, scan);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sources() {
        assert_eq!(
            detect_source("https://github.com/o/r/issues/1"),
            TicketSource::Github
        );
        assert_eq!(
            detect_source("https://gitlab.com/g/p/-/issues/9"),
            TicketSource::Gitlab
        );
        assert_eq!(detect_source("PROJ-123"), TicketSource::Jira);
        assert_eq!(
            detect_source("https://x.atlassian.net/browse/AB-9"),
            TicketSource::Jira
        );
        assert_eq!(detect_source("42"), TicketSource::Github);
        assert_eq!(detect_source("do the thing"), TicketSource::Inline);
    }

    #[test]
    fn adf_renders_structured_markdown() {
        let doc = serde_json::json!({
            "type": "doc",
            "content": [
                {
                    "type": "heading",
                    "attrs": { "level": 2 },
                    "content": [{ "type": "text", "text": "Acceptance Criteria" }]
                },
                {
                    "type": "paragraph",
                    "content": [{ "type": "text", "text": "Calendar day cells hide meal lines." }]
                },
                {
                    "type": "bulletList",
                    "content": [
                        {
                            "type": "listItem",
                            "content": [{
                                "type": "paragraph",
                                "content": [
                                    { "type": "text", "text": "Meals with " },
                                    { "type": "text", "text": "reservations", "marks": [{ "type": "strong" }] },
                                    { "type": "text", "text": " still show." }
                                ]
                            }]
                        }
                    ]
                }
            ]
        });
        let md = flatten_adf_text(&doc);
        assert!(md.contains("## Acceptance Criteria"), "heading: {md}");
        assert!(md.contains("\n\n"), "blank-line separation: {md}");
        assert!(md.contains("- Meals with **reservations** still show."), "bullet+bold: {md}");
        // Headers no longer glue onto following text.
        assert!(!md.contains("CriteriaCalendar"), "no glued text: {md}");
    }

    #[test]
    fn jira_browse_url_derives_host_from_self() {
        // REST `self` link → clickable browse URL, never the API endpoint.
        assert_eq!(
            jira_browse_url(
                "PROJ-1",
                "PROJ-1",
                Some("https://co.atlassian.net/rest/api/3/issue/160804")
            ),
            Some("https://co.atlassian.net/browse/PROJ-1".into())
        );
        // browse-style raw input is normalized to the key.
        assert_eq!(
            jira_browse_url("AB-9", "https://x.atlassian.net/browse/AB-1", None),
            Some("https://x.atlassian.net/browse/AB-9".into())
        );
        // no host available → None (caller falls back).
        assert_eq!(jira_browse_url("AB-9", "AB-9", None), None);
    }

    #[test]
    fn jira_key_parse() {
        assert!(is_jira_key("PROJ-123"));
        assert!(is_jira_key("AB-1"));
        assert!(!is_jira_key("proj-123"));
        assert!(!is_jira_key("P-1"));
        assert_eq!(
            extract_jira_key_from_text("feat/PROJ-99-login"),
            Some("PROJ-99".into())
        );
    }

    #[test]
    fn inline_fetch() {
        let r = fetch_inline("Build dark mode\n\nDetails here", None).unwrap();
        assert_eq!(r.source, "inline");
        assert_eq!(r.title, "Build dark mode");
    }

    #[test]
    fn bearer_token_rejects_acli_help_dump() {
        let help = "Authenticate to use Atlassian CLI.\n\nUsage:\n  acli auth [command]\n";
        assert!(!looks_like_bearer_token(help));
        assert!(!looks_like_bearer_token("short"));
        assert!(!looks_like_bearer_token("token with spaces that are long enough!!"));
        assert!(looks_like_bearer_token(
            "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.abc.def"
        ));
    }

    #[test]
    fn parses_acli_jira_profile_yaml() {
        let yaml = r#"
version: 1
current_profile: d5b2094b-04bb-467d-b98e-f39df372f11b:712020:e26bdb9e-a73f-4a07-b3df-7b9a1b76136e
profiles:
    - site: tablecheck.atlassian.net
      cloud_id: d5b2094b-04bb-467d-b98e-f39df372f11b
      account_id: 712020:e26bdb9e-a73f-4a07-b3df-7b9a1b76136e
      email: alex@example.com
"#;
        let p = parse_acli_jira_profile_yaml(yaml).unwrap();
        assert_eq!(p.cloud_id, "d5b2094b-04bb-467d-b98e-f39df372f11b");
        assert_eq!(p.account_id, "712020:e26bdb9e-a73f-4a07-b3df-7b9a1b76136e");
        assert_eq!(p.site, "tablecheck.atlassian.net");
        assert_eq!(p.email, "alex@example.com");
    }

    #[test]
    fn gateway_url_for_bearer() {
        let att = serde_json::json!({"id": "117160", "filename": "x.png"});
        let auth = JiraDownloadAuth::Bearer("eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.abc.def".into());
        let url = attachment_download_url(
            Some("d5b2094b-04bb-467d-b98e-f39df372f11b"),
            Some("tablecheck.atlassian.net"),
            &att,
            &auth,
        )
        .unwrap();
        assert_eq!(
            url,
            "https://api.atlassian.com/ex/jira/d5b2094b-04bb-467d-b98e-f39df372f11b/rest/api/3/attachment/content/117160"
        );
    }

    #[test]
    fn decodes_go_keyring_oauth_blob() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let payload = serde_json::json!({
            "access_token": "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.abc.def",
            "token_type": "Bearer",
            "refresh_token": "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.refresh.tok",
            "expiry": "2026-07-15T00:00:00.000Z",
            "expires_in": 3600
        });
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(payload.to_string().as_bytes()).unwrap();
        let compressed = enc.finish().unwrap();
        let raw = format!("go-keyring-base64:{}", B64.encode(compressed));
        let tok = decode_go_keyring_access_token(&raw).unwrap();
        assert_eq!(tok, "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.abc.def");
    }
}
