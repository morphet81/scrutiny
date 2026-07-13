use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{ensure_config, find_shipped_default, load_config, SuggestedForge};
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
    let suggested = cfg.suggested_forge(&client);

    let (source, raw_input) = resolve_source(&input)?;
    let mut report = match source {
        TicketSource::Inline => fetch_inline(&raw_input, input.title.as_deref())?,
        TicketSource::Jira => fetch_jira(&input.cwd, &raw_input)?,
        TicketSource::Github => fetch_github(&input.cwd, &raw_input)?,
        TicketSource::Gitlab => fetch_gitlab(&input.cwd, &raw_input)?,
    };
    report.suggested_forge = suggested;
    report.figma_urls = extract_figma_urls(&report);

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
    ensure_cmd("acli")?;
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
    let url = raw_json
        .get("self")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
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

fn flatten_adf_text(v: &Value) -> String {
    let mut out = String::new();
    flatten_adf_rec(v, &mut out);
    out
}

fn flatten_adf_rec(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(s);
        }
        Value::Array(arr) => {
            for item in arr {
                flatten_adf_rec(item, out);
            }
        }
        Value::Object(map) => {
            if let Some(t) = map.get("type").and_then(|x| x.as_str()) {
                if t == "paragraph" || t == "heading" || t == "listItem" {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
            if let Some(text) = map.get("text").and_then(|x| x.as_str()) {
                out.push_str(text);
            }
            if let Some(content) = map.get("content") {
                flatten_adf_rec(content, out);
            }
        }
        _ => {}
    }
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
    let dir = std::env::temp_dir().join(format!("{key}-attachments"));
    fs::create_dir_all(&dir).context("create attachments dir")?;

    let token = Command::new("acli")
        .args(["auth", "token"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let Some(token) = token else {
        // Keep dir path even if we cannot download
        return Ok(Some(dir.display().to_string()));
    };

    for att in attachments {
        let filename = att
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("attachment.bin");
        let content_url = att.get("content").and_then(|v| v.as_str());
        let Some(url) = content_url else {
            continue;
        };
        let dest = dir.join(filename);
        let status = Command::new("curl")
            .args([
                "-s",
                "-L",
                "-H",
                &format!("Authorization: Bearer {token}"),
                "-o",
                &dest.display().to_string(),
                url,
            ])
            .status();
        let _ = status; // best-effort
    }
    Ok(Some(dir.display().to_string()))
}

fn fetch_github(cwd: &Path, raw: &str) -> Result<TicketReport> {
    ensure_cmd("gh")?;
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
    ensure_cmd("glab")?;
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
    urls
}

fn ensure_cmd(name: &str) -> Result<()> {
    let status = Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !status {
        // Windows: where
        let status = Command::new(name)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !status {
            bail!("{name} not found on PATH. Install and authenticate it before forge-fetch.");
        }
    }
    Ok(())
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
}
