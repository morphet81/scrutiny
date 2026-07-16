//! `scrutiny pr` — standalone PR creation. Fetches a ticket (or infers/prompts),
//! suggests a title + description from it, lets the user confirm/edit both, then
//! creates the PR. No implement pipeline, no AI.

use anyhow::{bail, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input};
use serde::Serialize;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::config::{ensure_config, find_shipped_default, load_config, Config};
use crate::forge::fetch::{run_forge_fetch, ForgeFetchInput, TicketReport};
use crate::forge::scaffold;
use crate::git::{self, git_ok, git_stdout};
use crate::paths::{artifact_path, prepare_artifacts, write_json_pretty};
use crate::pr;

#[derive(Debug, Clone)]
pub struct PrCmdInput {
    pub cwd: PathBuf,
    /// Ticket URL / key / number. None → infer from branch, else prompt.
    pub ticket: Option<String>,
    /// Force ticket source (jira|github|gitlab|inline).
    pub source: Option<String>,
    /// Create a ready PR instead of the default draft.
    pub ready: bool,
    /// Skip interactive prompts.
    pub non_interactive: bool,
}

#[derive(Debug, Serialize)]
struct PrSummary {
    pr_url: String,
    base: String,
    title: String,
    draft: bool,
    ticket: Option<String>,
}

pub fn run_pr(input: PrCmdInput) -> Result<PathBuf> {
    let cwd = input.cwd.clone();
    let dir = prepare_artifacts(&cwd, None, &[])?;

    let shipped = find_shipped_default(&cwd);
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;

    preflight_branch(&cwd, &cfg, input.non_interactive)?;

    let ticket = resolve_ticket(&cwd, input.ticket.as_deref(), input.source.as_deref(), input.non_interactive);

    let (suggested_title, suggested_body, ticket_id) = match &ticket {
        Some(t) => {
            let prefix = scaffold::guess_prefix(t);
            (
                scaffold::guess_pr_title(t, prefix),
                scaffold::guess_pr_body(t),
                t.url.clone().or_else(|| Some(t.id.clone())),
            )
        }
        None => (title_from_branch(&cwd), String::new(), None),
    };

    let choice = pr::confirm_pr_meta(
        &cfg,
        &cwd,
        &dir,
        &suggested_title,
        &suggested_body,
        input.non_interactive,
    )?;

    let draft = !input.ready;
    let url = pr::create_pr(&cwd, &dir, &choice.base, &choice.title, &choice.body, draft)?;
    eprintln!(
        "scrutiny pr: {} PR → {url}",
        if draft { "draft" } else { "ready" }
    );

    let summary = PrSummary {
        pr_url: url,
        base: choice.base,
        title: choice.title,
        draft,
        ticket: ticket_id,
    };
    let out = artifact_path("pr");
    write_json_pretty(&out, &summary)?;
    Ok(out)
}

/// What push is needed before opening the PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushNeed {
    /// Remote is up to date.
    None,
    /// Branch has no upstream — needs a first `push -u`.
    FirstPush,
    /// Upstream exists but HEAD is `n` commits ahead.
    Ahead(usize),
}

/// Pure decision: given upstream presence and how far HEAD is ahead of it.
fn push_need(has_upstream: bool, ahead_of_upstream: usize) -> PushNeed {
    if !has_upstream {
        PushNeed::FirstPush
    } else if ahead_of_upstream > 0 {
        PushNeed::Ahead(ahead_of_upstream)
    } else {
        PushNeed::None
    }
}

/// Gate PR creation on branch state: no-commits (hard fail), dirty tree
/// (confirm), and unpushed commits (confirm → push). See the plan for rules.
fn preflight_branch(cwd: &Path, cfg: &Config, non_interactive: bool) -> Result<()> {
    let interactive = !non_interactive
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal();

    // 1) No commits vs base → fail early (skip if base can't be resolved).
    if let Ok(base) = git::resolve_base_branch(cwd, &cfg.git.base_candidates, None) {
        if git::commits_ahead(cwd, &base) == 0 {
            let branch = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
                .unwrap_or_else(|_| "HEAD".into())
                .trim()
                .to_string();
            bail!("no commits between {branch} and {base} — nothing to open a PR for");
        }
    }

    // 2) Dirty tree → confirm (they won't be in the PR).
    if git::is_dirty(cwd) {
        if interactive {
            let go = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Uncommitted changes won't be in the PR. Continue?")
                .default(false)
                .interact()?;
            if !go {
                bail!("aborted: commit or stash your changes first");
            }
        } else {
            eprintln!("scrutiny pr: warning — uncommitted changes won't be in the PR");
        }
    }

    // 3) Unpushed / never-pushed commits → confirm, then push.
    let has_upstream = git_ok(cwd, &["rev-parse", "--abbrev-ref", "@{upstream}"]);
    let ahead = if has_upstream {
        git::commits_ahead(cwd, "@{upstream}")
    } else {
        0
    };
    match push_need(has_upstream, ahead) {
        PushNeed::None => {}
        need => {
            if interactive {
                let prompt = match need {
                    PushNeed::FirstPush => {
                        "Branch not pushed to origin. Push and continue?".to_string()
                    }
                    PushNeed::Ahead(n) => {
                        let upstream = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "@{upstream}"])
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        format!("{n} local commit(s) not pushed to {upstream}. Push and continue?")
                    }
                    PushNeed::None => unreachable!(),
                };
                let go = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(prompt)
                    .default(true)
                    .interact()?;
                if !go {
                    bail!("aborted: push your commits first");
                }
            }
            pr::push_current_branch(cwd)?;
        }
    }

    Ok(())
}

/// Fetch the ticket: explicit arg → infer from branch → prompt (TTY) → none.
fn resolve_ticket(
    cwd: &Path,
    ticket: Option<&str>,
    source: Option<&str>,
    non_interactive: bool,
) -> Option<TicketReport> {
    if let Some(raw) = ticket.map(str::trim).filter(|s| !s.is_empty()) {
        return fetch_ticket(cwd, raw, source);
    }

    // No arg: try inferring a Jira key from the branch name.
    if let Ok((report, _)) = run_forge_fetch(fetch_input(cwd, None, source)) {
        return Some(report);
    }

    // Branch gave nothing: reuse a ticket already fetched into .scrutiny/.
    if let Some(report) = cached_ticket(cwd) {
        eprintln!("scrutiny pr: reusing ticket {} from .scrutiny (cached)", report.id);
        return Some(report);
    }

    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if non_interactive || !tty {
        eprintln!("scrutiny pr: no ticket — suggestions from branch name only");
        return None;
    }

    let entered: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Ticket URL/key (empty = skip)")
        .allow_empty(true)
        .interact_text()
        .unwrap_or_default();
    let entered = entered.trim();
    if entered.is_empty() {
        None
    } else {
        fetch_ticket(cwd, entered, source)
    }
}

fn fetch_ticket(cwd: &Path, raw: &str, source: Option<&str>) -> Option<TicketReport> {
    match run_forge_fetch(fetch_input(cwd, Some(raw.to_string()), source)) {
        Ok((report, _)) => Some(report),
        Err(e) => {
            eprintln!("scrutiny pr: ticket fetch failed ({e:#}) — continuing without ticket");
            None
        }
    }
}

fn fetch_input(cwd: &Path, input: Option<String>, source: Option<&str>) -> ForgeFetchInput {
    ForgeFetchInput {
        cwd: cwd.to_path_buf(),
        input,
        source: source.map(str::to_string),
        inline: false,
        client: None,
        title: None,
    }
}

/// Read every `.scrutiny/*/ticket.json` into a `TicketReport` (skip unreadable/invalid).
fn load_cached_tickets(cwd: &Path) -> Vec<TicketReport> {
    let mut reports = Vec::new();
    let Ok(entries) = std::fs::read_dir(cwd.join(".scrutiny")) else {
        return reports;
    };
    for entry in entries.flatten() {
        let path = entry.path().join("ticket.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(report) = serde_json::from_str::<TicketReport>(&text) {
                reports.push(report);
            }
        }
    }
    reports
}

/// Reuse a previously fetched ticket from `.scrutiny/` when arg + branch gave nothing.
fn cached_ticket(cwd: &Path) -> Option<TicketReport> {
    let branch = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    pick_cached_ticket(load_cached_tickets(cwd), branch.trim())
}

/// Pure selection: branch-matched id wins; else the single cached ticket; else None (ambiguous).
fn pick_cached_ticket(mut reports: Vec<TicketReport>, branch: &str) -> Option<TicketReport> {
    let up = branch.to_uppercase();
    if let Some(i) = reports
        .iter()
        .position(|r| !r.id.is_empty() && up.contains(&r.id.to_uppercase()))
    {
        return Some(reports.swap_remove(i));
    }
    if reports.len() == 1 {
        return reports.pop();
    }
    None
}

/// Fallback title when no ticket: humanize the current branch name.
fn title_from_branch(cwd: &Path) -> String {
    let branch = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();
    let tail = branch.rsplit('/').next().unwrap_or(&branch);
    let humanized = tail.replace(['-', '_'], " ");
    humanized.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_need_cases() {
        assert_eq!(push_need(false, 0), PushNeed::FirstPush);
        assert_eq!(push_need(false, 3), PushNeed::FirstPush);
        assert_eq!(push_need(true, 0), PushNeed::None);
        assert_eq!(push_need(true, 2), PushNeed::Ahead(2));
    }

    fn report(id: &str) -> TicketReport {
        TicketReport {
            version: 1,
            source: "jira".into(),
            id: id.into(),
            url: None,
            title: String::new(),
            description: String::new(),
            labels: vec![],
            comments: vec![],
            attachments_dir: None,
            figma_urls: vec![],
            figma_dir: None,
            fields: serde_json::Value::Null,
            raw_path: None,
            fetched_at: String::new(),
            suggested_forge: Default::default(),
        }
    }

    #[test]
    fn cached_ticket_branch_match_is_case_insensitive() {
        let picked = pick_cached_ticket(vec![report("NERO-531")], "new-tc-manager/feat/nero-531");
        assert_eq!(picked.map(|r| r.id), Some("NERO-531".into()));
    }

    #[test]
    fn cached_ticket_prefers_branch_match_over_others() {
        let picked = pick_cached_ticket(
            vec![report("ABC-1"), report("NERO-531")],
            "feat/nero-531",
        );
        assert_eq!(picked.map(|r| r.id), Some("NERO-531".into()));
    }

    #[test]
    fn cached_ticket_ambiguous_without_branch_match_is_none() {
        let picked = pick_cached_ticket(vec![report("ABC-1"), report("XYZ-2")], "feat/unrelated");
        assert!(picked.is_none());
    }

    #[test]
    fn cached_ticket_single_is_reused_without_branch_match() {
        let picked = pick_cached_ticket(vec![report("ABC-1")], "feat/unrelated");
        assert_eq!(picked.map(|r| r.id), Some("ABC-1".into()));
    }
}
