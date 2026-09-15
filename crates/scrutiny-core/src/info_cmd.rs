//! `scrutiny info` — colorful ticket summary (Jira) + related GitHub PRs.

use anyhow::{bail, Result};
use console::Style;
use serde::Deserialize;
use serde_json::Value;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::forge::fetch::{
    apply_jira_field_names, extract_jira_custom_text_fields, jira_key_from_branch,
    jira_key_from_url_or_raw, load_jira_field_names, run_forge_fetch, ForgeFetchInput,
    JiraCustomTextField, TicketReport,
};
use crate::mdterm::render_markdown;

#[derive(Debug, Clone)]
pub struct InfoCmdInput {
    pub cwd: PathBuf,
    /// Jira URL, key, or empty → detect from current branch.
    pub input: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GhPr {
    number: u64,
    title: String,
    url: String,
    state: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(default, rename = "isDraft")]
    is_draft: bool,
}

/// Fetch ticket + PRs and print a colorful terminal report. Returns ticket key.
pub fn run_info(input: InfoCmdInput) -> Result<String> {
    let cwd = input.cwd;
    let raw = resolve_ticket_input(&cwd, input.input.as_deref())?;
    let key = jira_key_from_url_or_raw(&raw)?;

    let (ticket, _) = run_forge_fetch(ForgeFetchInput {
        cwd: cwd.clone(),
        input: Some(raw),
        source: Some("jira".into()),
        inline: false,
        client: None,
        title: None,
    })?;

    let mut custom = extract_jira_custom_text_fields(&ticket.fields);
    let names = load_jira_field_names(&cwd);
    apply_jira_field_names(&mut custom, &names);

    let prs = find_related_prs(&cwd, &key);

    print_info_report(&ticket, &custom, &prs);
    Ok(key)
}

fn resolve_ticket_input(cwd: &Path, input: Option<&str>) -> Result<String> {
    if let Some(raw) = input.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(raw.to_string());
    }
    if let Some(key) = jira_key_from_branch(cwd)? {
        return Ok(key);
    }
    bail!(
        "scrutiny info needs a Jira key/URL, or a branch containing KEY-123 \
         (e.g. new-tc-manager/nero-730)"
    )
}

fn find_related_prs(cwd: &Path, key: &str) -> Vec<GhPr> {
    let key_lower = key.to_ascii_lowercase();
    let mut found: Vec<GhPr> = Vec::new();

    // Search by key across open + closed.
    if let Some(list) = gh_pr_list(cwd, &["--state", "all", "--search", key, "--limit", "20"]) {
        for pr in list {
            if pr_matches_key(&pr, &key_lower) {
                found.push(pr);
            }
        }
    }

    // Also: PR for current branch if head mentions the key.
    if let Some(pr) = gh_pr_view_current(cwd) {
        if pr_matches_key(&pr, &key_lower) && !found.iter().any(|p| p.number == pr.number) {
            found.push(pr);
        }
    }

    found.sort_by(|a, b| b.number.cmp(&a.number));
    found
}

fn pr_matches_key(pr: &GhPr, key_lower: &str) -> bool {
    pr.title.to_ascii_lowercase().contains(key_lower)
        || pr.head_ref_name.to_ascii_lowercase().contains(key_lower)
        || pr
            .head_ref_name
            .to_ascii_lowercase()
            .contains(&key_lower.replace('-', "/"))
        || pr
            .url
            .to_ascii_lowercase()
            .contains(key_lower)
}

fn gh_pr_list(cwd: &Path, extra: &[&str]) -> Option<Vec<GhPr>> {
    let mut args = vec![
        "pr",
        "list",
        "--json",
        "number,title,url,state,headRefName,isDraft",
    ];
    args.extend_from_slice(extra);
    let out = Command::new("gh")
        .args(&args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn gh_pr_view_current(cwd: &Path) -> Option<GhPr> {
    let out = Command::new("gh")
        .args([
            "pr",
            "view",
            "--json",
            "number,title,url,state,headRefName,isDraft",
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn print_info_report(ticket: &TicketReport, custom: &[JiraCustomTextField], prs: &[GhPr]) {
    let color = std::io::stderr().is_terminal();
    let ok = style(Style::new().green().bold(), color);
    let title = style(Style::new().cyan().bold(), color);
    let label = style(Style::new().yellow().bold(), color);
    let dim = style(Style::new().dim(), color);
    let link = style(Style::new().blue().underlined(), color);
    let rule = ok.apply_to("══════════════════════════════════════════════════════════");

    let status = field_status_name(&ticket.fields);
    let assignee = field_assignee_name(&ticket.fields);

    eprintln!();
    eprintln!("{rule}");
    eprintln!("{}  {}", ok.apply_to("TICKET"), title.apply_to(&ticket.id));
    if !ticket.title.trim().is_empty() {
        eprintln!("  {}", title.apply_to(ticket.title.trim()));
    }
    eprintln!("{rule}");

    if let Some(url) = ticket.url.as_deref().filter(|u| !u.is_empty()) {
        eprintln!("{}  {}", label.apply_to("URL"), link.apply_to(url));
    }
    if let Some(s) = status {
        eprintln!("{}  {}", label.apply_to("Status"), ok.apply_to(s));
    }
    if let Some(a) = assignee {
        eprintln!("{}  {}", label.apply_to("Assignee"), dim.apply_to(a));
    }
    if !ticket.labels.is_empty() {
        eprintln!(
            "{}  {}",
            label.apply_to("Labels"),
            dim.apply_to(ticket.labels.join(", "))
        );
    }

    eprintln!();
    eprintln!("{}", label.apply_to("Summary"));
    eprintln!("  {}", ticket.title.trim());

    eprintln!();
    eprintln!("{}", label.apply_to("Description"));
    let desc = ticket.description.trim();
    if desc.is_empty() {
        eprintln!("  {}", dim.apply_to("(empty)"));
    } else {
        for line in render_markdown(desc).lines() {
            eprintln!("  {line}");
        }
    }

    if !custom.is_empty() {
        eprintln!();
        eprintln!("{}", label.apply_to("Extra fields"));
        for f in custom {
            let name = if f.name == f.id {
                format!("{} ({})", "Custom field", f.id)
            } else {
                f.name.clone()
            };
            eprintln!();
            eprintln!("  {}", title.apply_to(&name));
            for line in render_markdown(&f.text).lines() {
                eprintln!("    {line}");
            }
        }
    }

    eprintln!();
    eprintln!("{}", label.apply_to("Pull requests"));
    if prs.is_empty() {
        eprintln!("  {}", dim.apply_to("(none found via gh)"));
    } else {
        for pr in prs {
            let state = pr_state_label(pr);
            let state_style = match pr.state.to_ascii_uppercase().as_str() {
                "OPEN" if pr.is_draft => style(Style::new().yellow().bold(), color),
                "OPEN" => style(Style::new().green().bold(), color),
                "MERGED" => style(Style::new().magenta().bold(), color),
                "CLOSED" => style(Style::new().red().bold(), color),
                _ => dim.clone(),
            };
            eprintln!(
                "  {}  {}  {}",
                state_style.apply_to(format!("{:<8}", state)),
                title.apply_to(format!("#{}", pr.number)),
                link.apply_to(&pr.url)
            );
            eprintln!("         {}", dim.apply_to(&pr.title));
            if !pr.head_ref_name.is_empty() {
                eprintln!("         {}", dim.apply_to(format!("branch {}", pr.head_ref_name)));
            }
        }
    }

    eprintln!("{rule}");
    eprintln!();
}

fn style(s: Style, color: bool) -> Style {
    if color {
        s
    } else {
        Style::new()
    }
}

fn pr_state_label(pr: &GhPr) -> String {
    let s = pr.state.to_ascii_uppercase();
    if s == "OPEN" && pr.is_draft {
        "DRAFT".into()
    } else {
        s
    }
}

fn field_status_name(fields: &Value) -> Option<String> {
    fields
        .pointer("/status/name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn field_assignee_name(fields: &Value) -> Option<String> {
    fields
        .pointer("/assignee/displayName")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn custom_text_fields_extract_adf() {
        let fields = json!({
            "customfield_10887": {
                "type": "doc",
                "version": 1,
                "content": [{
                    "type": "paragraph",
                    "content": [{"type": "text", "text": "Expected toast copy."}]
                }]
            },
            "customfield_10019": "2|i06uja:zhzr",
            "summary": "ignored"
        });
        let got = extract_jira_custom_text_fields(&fields);
        assert_eq!(got.len(), 1);
        assert!(got[0].text.contains("Expected toast"));
        assert_eq!(got[0].id, "customfield_10887");
    }

    #[test]
    fn pr_matches_key_on_branch_or_title() {
        let pr = GhPr {
            number: 1,
            title: "fix: waitlist toast".into(),
            url: "https://github.com/o/r/pull/1".into(),
            state: "OPEN".into(),
            head_ref_name: "new-tc-manager/nero-730".into(),
            is_draft: false,
        };
        assert!(pr_matches_key(&pr, "nero-730"));
        assert!(!pr_matches_key(&pr, "nero-999"));
    }

    #[test]
    fn resolve_ticket_input_uses_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let got = resolve_ticket_input(dir.path(), Some("  NERO-730  ")).unwrap();
        assert_eq!(got, "NERO-730");
    }
}
