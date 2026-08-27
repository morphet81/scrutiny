use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

use crate::gh::gh_output_retry;
use crate::git::git_stdout;
use crate::review_cmd::{run_pending_triage, run_review, ReviewCmdInput};

pub struct ProbeStackInput {
    pub cwd: PathBuf,
    pub stack_number: Option<u64>,
    pub client: Option<String>,
    pub spawn_mode: Option<String>,
    pub from_json: Option<String>,
    pub skip_agents: bool,
    pub event: Option<String>,
    pub non_interactive: bool,
}

#[derive(Deserialize)]
struct GhStackView {
    branches: Vec<GhStackBranch>,
}

#[derive(Deserialize)]
struct GhStackBranch {
    name: String,
    #[serde(rename = "isMerged", default)]
    is_merged: bool,
    pr: Option<GhStackPr>,
}

#[derive(Deserialize)]
struct GhStackPr {
    number: u64,
    state: String,
}

pub fn run_probe_stack(input: ProbeStackInput) -> Result<Vec<PathBuf>> {
    let cwd = &input.cwd;

    let orig_branch = if input.stack_number.is_some() {
        let branch = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
            .context("get current branch")?;
        let branch = branch.trim().to_string();
        let n = input.stack_number.unwrap().to_string();
        let out = gh_output_retry(cwd, &["stack", "checkout", &n], None)
            .context("gh stack checkout")?;
        if !out.status.success() {
            bail!(
                "gh stack checkout {} failed: {}",
                n,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Some(branch)
    } else {
        None
    };

    let view_out = gh_output_retry(cwd, &["stack", "view", "--json"], None)
        .context("gh stack view --json")?;

    if let Some(ref orig) = orig_branch {
        git_stdout(cwd, &["checkout", orig]).ok();
    }

    if !view_out.status.success() {
        bail!(
            "gh stack view --json failed: {}",
            String::from_utf8_lossy(&view_out.stderr)
        );
    }

    let view: GhStackView =
        serde_json::from_slice(&view_out.stdout).context("parse gh stack view JSON")?;

    let open: Vec<&GhStackBranch> = view
        .branches
        .iter()
        .filter(|b| {
            !b.is_merged
                && b.pr
                    .as_ref()
                    .map(|p| p.state.eq_ignore_ascii_case("OPEN"))
                    .unwrap_or(false)
        })
        .collect();

    if open.is_empty() {
        bail!("no open PRs found in stack");
    }

    eprintln!("scrutiny probe stack: {} open PR(s)", open.len());

    // Phase 1: run all reviews unattended (settings prompted once from PR #1).
    let mut reuse_answers = input.from_json.clone();
    let mut paths = Vec::new();
    let mut pending_triages = Vec::new();
    for (i, branch) in open.iter().enumerate() {
        let pr_number = branch.pr.as_ref().unwrap().number;
        eprintln!(
            "scrutiny probe stack [{}/{}]: review PR #{} ({}) …",
            i + 1,
            open.len(),
            pr_number,
            branch.name
        );
        let result = run_review(ReviewCmdInput {
            cwd: input.cwd.clone(),
            pr: Some(pr_number.to_string()),
            client: input.client.clone(),
            spawn_mode: input.spawn_mode.clone(),
            from_json: reuse_answers.clone(),
            skip_agents: input.skip_agents,
            event: input.event.clone(),
            non_interactive: input.non_interactive,
            from_report: None,
            scan_path: None,
            skip_triage: true,
        })?;
        if reuse_answers.is_none() {
            reuse_answers = result.answers_json;
        }
        paths.push(result.findings_path);
        if let Some(t) = result.pending_triage {
            pending_triages.push(t);
        }
    }

    // Phase 2: triage findings one PR at a time.
    let total = pending_triages.len();
    for (i, triage) in pending_triages.into_iter().enumerate() {
        eprintln!(
            "scrutiny probe stack [{}/{}]: triage findings …",
            i + 1,
            total,
        );
        run_pending_triage(triage)?;
    }

    Ok(paths)
}
