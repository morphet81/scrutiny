//! `scrutiny pr` — standalone PR creation. Fetches a ticket (or infers/prompts),
//! suggests a title + description from it, lets the user confirm/edit both, then
//! creates the PR. No implement pipeline, no AI.

use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Input};
use serde::Serialize;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::config::{ensure_config, find_shipped_default, load_config};
use crate::forge::fetch::{run_forge_fetch, ForgeFetchInput, TicketReport};
use crate::forge::scaffold;
use crate::git::git_stdout;
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
