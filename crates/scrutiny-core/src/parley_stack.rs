//! `scrutiny parley stack` — bottom→top parley over a `gh stack`, defer push.

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm};
use serde::Deserialize;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::agent_runner::{run_headless, run_nonheadless, wait_for_sentinels, HeadlessKind};
use crate::config::{ensure_config, find_shipped_default, load_config};
use crate::gh::gh_output_retry;
use crate::git::git_stdout;
use crate::parley::pr_has_unresolved_comments;
use crate::parley_cmd::{run_parley, ParleyCmdInput};
use crate::paths::prepare_artifacts;
use crate::runtime::{resolve_client, DetectedClient, ResolveClientInput};
use crate::terminal::{
    force_close_agent_panes, preflight_zellij_agent_panes, resolve_terminal, AgentPaneCleanupGuard,
};

/// Max agent-resolve + continue rounds for one rebase conflict sequence.
const MAX_CONFLICT_ROUNDS: u32 = 5;

pub struct ParleyStackInput {
    pub cwd: PathBuf,
    pub stack_number: Option<u64>,
    pub client: Option<String>,
    pub spawn_mode: Option<String>,
    pub from_json: Option<String>,
    pub skip_agents: bool,
    pub skip_ship: bool,
    pub non_interactive: bool,
}

#[derive(Deserialize)]
struct GhStackView {
    trunk: String,
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

struct StackLayer {
    name: String,
    pr_number: u64,
    /// Parent branch tip to rebase onto (trunk or previous stack branch).
    parent: String,
}

#[derive(Debug, Clone, Copy)]
enum ContinueCmd {
    /// `git rebase --continue`
    GitRebase,
    /// `gh stack rebase --continue`
    GhStackRebase,
}

pub fn run_parley_stack(input: ParleyStackInput) -> Result<Vec<PathBuf>> {
    let cwd = &input.cwd;

    let orig_branch = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
        .context("get current branch")?
        .trim()
        .to_string();

    if let Some(n) = input.stack_number {
        let n = n.to_string();
        let out = gh_output_retry(cwd, &["stack", "checkout", &n], None)
            .context("gh stack checkout")?;
        if !out.status.success() {
            bail!(
                "gh stack checkout {} failed: {}",
                n,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    // Human view (stderr of gh goes to our stderr; short is non-TUI).
    let short = gh_output_retry(cwd, &["stack", "view", "--short"], None)
        .context("gh stack view --short")?;
    if short.status.success() {
        let text = String::from_utf8_lossy(&short.stdout);
        if !text.trim().is_empty() {
            eprintln!("scrutiny parley stack:\n{}", text.trim_end());
        }
    }

    let view_out = gh_output_retry(cwd, &["stack", "view", "--json"], None)
        .context("gh stack view --json")?;
    if !view_out.status.success() {
        // Restore checkout target if we switched stacks.
        let _ = git_stdout(cwd, &["checkout", &orig_branch]);
        bail!(
            "gh stack view --json failed: {}",
            String::from_utf8_lossy(&view_out.stderr)
        );
    }

    let view: GhStackView =
        serde_json::from_slice(&view_out.stdout).context("parse gh stack view JSON")?;

    let layers = open_layers(&view);

    if layers.is_empty() {
        let _ = git_stdout(cwd, &["checkout", &orig_branch]);
        bail!("no open PRs found in stack");
    }

    eprintln!(
        "scrutiny parley stack: {} open PR(s), bottom→top, autonomous (no push until end)",
        layers.len()
    );

    let mut paths = Vec::new();

    for (i, layer) in layers.iter().enumerate() {
        eprintln!(
            "scrutiny parley stack [{}/{}]: {} (PR #{}) — rebase onto {} …",
            i + 1,
            layers.len(),
            layer.name,
            layer.pr_number,
            layer.parent
        );

        if let Err(e) = git_stdout(cwd, &["checkout", &layer.name]) {
            bail!(
                "parley stack stopped at {} (PR #{}): checkout failed: {e:#}",
                layer.name,
                layer.pr_number
            );
        }

        let rebase = Command::new("git")
            .args(["rebase", &layer.parent])
            .current_dir(cwd)
            .output()
            .with_context(|| format!("git rebase {}", layer.parent))?;
        if !rebase.status.success() {
            let combined = combine_output(&rebase.stdout, &rebase.stderr);
            if looks_like_rebase_conflict(&combined)
                || !list_unmerged_paths(cwd).unwrap_or_default().is_empty()
            {
                if let Err(e) = resolve_rebase_conflicts(
                    cwd,
                    &input,
                    ContinueCmd::GitRebase,
                    &format!(
                        "git rebase {} onto {} (PR #{})",
                        layer.name, layer.parent, layer.pr_number
                    ),
                    Some(layer.pr_number),
                    &combined,
                ) {
                    let _ = Command::new("git")
                        .args(["rebase", "--abort"])
                        .current_dir(cwd)
                        .output();
                    bail!(
                        "parley stack stopped at {} (PR #{}): rebase onto {} conflict unresolved: {e:#}",
                        layer.name,
                        layer.pr_number,
                        layer.parent
                    );
                }
            } else {
                let _ = Command::new("git")
                    .args(["rebase", "--abort"])
                    .current_dir(cwd)
                    .output();
                bail!(
                    "parley stack stopped at {} (PR #{}): rebase onto {} failed:\n{}",
                    layer.name,
                    layer.pr_number,
                    layer.parent,
                    combined.trim()
                );
            }
        }

        eprintln!(
            "scrutiny parley stack [{}/{}]: gh pr view — check unresolved comments on PR #{} …",
            i + 1,
            layers.len(),
            layer.pr_number
        );
        let needs_parley = match pr_has_unresolved_comments(cwd, layer.pr_number) {
            Ok(v) => v,
            Err(e) => {
                bail!(
                    "parley stack stopped at {} (PR #{}): comment check failed: {e:#}",
                    layer.name,
                    layer.pr_number
                );
            }
        };
        if !needs_parley {
            eprintln!(
                "scrutiny parley stack [{}/{}]: PR #{} — no unresolved comments, skip parley",
                i + 1,
                layers.len(),
                layer.pr_number
            );
        } else {
            eprintln!(
                "scrutiny parley stack [{}/{}]: parley PR #{} (commit, no push) …",
                i + 1,
                layers.len(),
                layer.pr_number
            );

            let path = match run_parley(ParleyCmdInput {
                cwd: input.cwd.clone(),
                pr: Some(layer.pr_number.to_string()),
                client: input.client.clone(),
                spawn_mode: input.spawn_mode.clone(),
                from_json: input.from_json.clone(),
                // Stack mode is fully autonomous for knobs.
                non_interactive: true,
                skip_agents: input.skip_agents,
                skip_ship: input.skip_ship,
                skip_push: true,
            }) {
                Ok(p) => p,
                Err(e) => {
                    bail!(
                        "parley stack stopped at {} (PR #{}): {e:#}",
                        layer.name,
                        layer.pr_number
                    );
                }
            };
            paths.push(path);
        }

        eprintln!(
            "scrutiny parley stack [{}/{}]: gh stack rebase …",
            i + 1,
            layers.len()
        );
        let rb = gh_output_retry(cwd, &["stack", "rebase"], None).context("gh stack rebase")?;
        if !rb.status.success() {
            let combined = combine_output(&rb.stdout, &rb.stderr);
            if looks_like_rebase_conflict(&combined)
                || !list_unmerged_paths(cwd).unwrap_or_default().is_empty()
            {
                if let Err(e) = resolve_rebase_conflicts(
                    cwd,
                    &input,
                    ContinueCmd::GhStackRebase,
                    &format!(
                        "gh stack rebase after PR #{} ({})",
                        layer.pr_number, layer.name
                    ),
                    Some(layer.pr_number),
                    &combined,
                ) {
                    bail!(
                        "parley stack stopped at {} (PR #{}): gh stack rebase conflict unresolved: {e:#}\n{}",
                        layer.name,
                        layer.pr_number,
                        combined.trim()
                    );
                }
            } else {
                bail!(
                    "parley stack stopped at {} (PR #{}): gh stack rebase failed: {}",
                    layer.name,
                    layer.pr_number,
                    combined.trim()
                );
            }
        }
    }

    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let do_push = if input.non_interactive {
        eprintln!("scrutiny parley stack: --yes — run `gh stack push`");
        true
    } else if tty {
        Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Run `gh stack push` now?")
            .default(true)
            .interact()
            .unwrap_or(false)
    } else {
        eprintln!("scrutiny parley stack: no TTY — skip `gh stack push` (run manually)");
        false
    };

    if do_push {
        eprintln!("scrutiny parley stack: gh stack push …");
        let push = gh_output_retry(cwd, &["stack", "push"], None).context("gh stack push")?;
        if !push.status.success() {
            let _ = git_stdout(cwd, &["checkout", &orig_branch]);
            bail!(
                "gh stack push failed: {}",
                String::from_utf8_lossy(&push.stderr)
            );
        }
        eprintln!("scrutiny parley stack: push complete");
    } else {
        eprintln!("scrutiny parley stack: left unpushed — run `gh stack push` when ready");
    }

    let _ = git_stdout(cwd, &["checkout", &orig_branch]);
    Ok(paths)
}

fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    let out = String::from_utf8_lossy(stdout);
    let err = String::from_utf8_lossy(stderr);
    match (out.trim().is_empty(), err.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => out.into_owned(),
        (true, false) => err.into_owned(),
        (false, false) => format!("{out}\n{err}"),
    }
}

fn looks_like_rebase_conflict(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("conflict")
        || lower.contains("could not apply")
        || lower.contains("rebase --continue")
        || lower.contains("stack rebase --continue")
        || lower.contains("fix conflicts")
        || lower.contains("<<<<<<")
}

fn list_unmerged_paths(cwd: &Path) -> Result<Vec<String>> {
    let out = Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(cwd)
        .output()
        .context("git diff --name-only --diff-filter=U")?;
    if !out.status.success() {
        // Fall back to ls-files -u unique paths.
        let out = Command::new("git")
            .args(["ls-files", "-u"])
            .current_dir(cwd)
            .output()
            .context("git ls-files -u")?;
        let mut paths = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let path = line.split('\t').nth(1).unwrap_or("").trim();
            if !path.is_empty() && !paths.iter().any(|p: &String| p == path) {
                paths.push(path.to_string());
            }
        }
        return Ok(paths);
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

fn rebase_in_progress(cwd: &Path) -> bool {
    let Ok(git_dir) = git_stdout(cwd, &["rev-parse", "--git-dir"]) else {
        return false;
    };
    let git_dir = PathBuf::from(git_dir.trim());
    let base = if git_dir.is_absolute() {
        git_dir
    } else {
        cwd.join(git_dir)
    };
    base.join("rebase-merge").is_dir() || base.join("rebase-apply").is_dir()
}

fn resolve_rebase_conflicts(
    cwd: &Path,
    input: &ParleyStackInput,
    continue_cmd: ContinueCmd,
    context: &str,
    pr: Option<u64>,
    initial_output: &str,
) -> Result<()> {
    let mut files = list_unmerged_paths(cwd)?;
    if files.is_empty() && !rebase_in_progress(cwd) {
        bail!("rebase failed but no unmerged paths / rebase state:\n{}", initial_output.trim());
    }

    if input.skip_agents {
        bail!(
            "rebase conflict during {context} (skip_agents set). Conflicted files:\n  {}\n\
             Resolve, then run `{}`",
            if files.is_empty() {
                "(see conflict output)".into()
            } else {
                files.join("\n  ")
            },
            continue_hint(continue_cmd)
        );
    }

    eprintln!("scrutiny parley stack: rebase conflict during {context}");
    if !files.is_empty() {
        eprintln!(
            "scrutiny parley stack: {} conflicted file(s):\n  {}",
            files.len(),
            files.join("\n  ")
        );
    }

    let shipped = find_shipped_default(&std::env::current_exe().unwrap_or_else(|_| cwd.to_path_buf()));
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;
    let detected = resolve_client(
        &cfg,
        ResolveClientInput {
            cli_override: input.client.clone(),
            skip_prompt: true,
        },
    )?;
    let session_model = cfg
        .models
        .get(&detected.client)
        .and_then(|m| m.m.clone().or(m.l.clone()).or(m.s.clone()))
        .unwrap_or_else(|| detected.client.clone());
    let model = cfg.resolve_agent_model(&detected.client, "parley_rebase_conflict", &session_model);
    let wall = Duration::from_secs(crate::timeouts::get().parley_prepush_fix);

    let session = pr.map(|n| n.to_string()).unwrap_or_else(|| "local".into());
    let _ = prepare_artifacts(cwd, Some(&session), &[]);

    for round in 1..=MAX_CONFLICT_ROUNDS {
        files = list_unmerged_paths(cwd)?;
        if files.is_empty() && !rebase_in_progress(cwd) {
            eprintln!("scrutiny parley stack: rebase conflict cleared");
            return Ok(());
        }
        if files.is_empty() && rebase_in_progress(cwd) {
            // Agent cleared markers / staged everything — just continue.
            eprintln!(
                "scrutiny parley stack: no unmerged paths — {}",
                continue_hint(continue_cmd)
            );
            match run_continue(cwd, continue_cmd)? {
                ContinueOutcome::Done => return Ok(()),
                ContinueOutcome::Conflict(out) => {
                    eprintln!(
                        "scrutiny parley stack: continue hit another conflict (round {round}/{MAX_CONFLICT_ROUNDS})"
                    );
                    if round == MAX_CONFLICT_ROUNDS {
                        bail!("still conflicting after {MAX_CONFLICT_ROUNDS} rounds:\n{out}");
                    }
                    continue;
                }
                ContinueOutcome::Failed(out) => {
                    bail!("{} failed:\n{out}", continue_hint(continue_cmd));
                }
            }
        }

        eprintln!(
            "scrutiny parley stack: spawning rebase-conflict agent \
             (client={}, model={model}, round {round}/{MAX_CONFLICT_ROUNDS})…",
            detected.client
        );
        spawn_conflict_agent(cwd, &cfg, &detected, &model, wall, &files, context)?;

        // Stage anything the agent resolved but forgot to add.
        let still = list_unmerged_paths(cwd)?;
        if !still.is_empty() {
            // Re-check: agent may have removed markers without git add.
            for f in &still {
                let path = cwd.join(f);
                if path.is_file() {
                    if let Ok(body) = std::fs::read_to_string(&path) {
                        if !body.contains("<<<<<<<") && !body.contains(">>>>>>>") {
                            let _ = Command::new("git")
                                .args(["add", "--", f])
                                .current_dir(cwd)
                                .status();
                        }
                    }
                }
            }
        }

        let after = list_unmerged_paths(cwd)?;
        if !after.is_empty() {
            if round == MAX_CONFLICT_ROUNDS {
                bail!(
                    "conflict agent left {} unmerged file(s) after {MAX_CONFLICT_ROUNDS} rounds:\n  {}",
                    after.len(),
                    after.join("\n  ")
                );
            }
            eprintln!(
                "scrutiny parley stack: {} file(s) still unmerged — retry agent",
                after.len()
            );
            continue;
        }

        eprintln!(
            "scrutiny parley stack: conflicts staged — {}",
            continue_hint(continue_cmd)
        );
        match run_continue(cwd, continue_cmd)? {
            ContinueOutcome::Done => {
                eprintln!("scrutiny parley stack: rebase continue ok");
                return Ok(());
            }
            ContinueOutcome::Conflict(out) => {
                eprintln!(
                    "scrutiny parley stack: continue hit another conflict (round {round}/{MAX_CONFLICT_ROUNDS})"
                );
                if round == MAX_CONFLICT_ROUNDS {
                    bail!("still conflicting after {MAX_CONFLICT_ROUNDS} rounds:\n{out}");
                }
            }
            ContinueOutcome::Failed(out) => {
                bail!("{} failed:\n{out}", continue_hint(continue_cmd));
            }
        }
    }

    bail!("rebase conflict unresolved after {MAX_CONFLICT_ROUNDS} rounds ({context})")
}

fn continue_hint(cmd: ContinueCmd) -> &'static str {
    match cmd {
        ContinueCmd::GitRebase => "git rebase --continue",
        ContinueCmd::GhStackRebase => "gh stack rebase --continue",
    }
}

enum ContinueOutcome {
    Done,
    Conflict(String),
    Failed(String),
}

fn run_continue(cwd: &Path, cmd: ContinueCmd) -> Result<ContinueOutcome> {
    let output = match cmd {
        ContinueCmd::GitRebase => {
            // Avoid editor for the continue commit message.
            Command::new("git")
                .args(["-c", "core.editor=true", "rebase", "--continue"])
                .current_dir(cwd)
                .output()
                .context("git rebase --continue")?
        }
        ContinueCmd::GhStackRebase => {
            gh_output_retry(cwd, &["stack", "rebase", "--continue"], None)
                .context("gh stack rebase --continue")?
        }
    };
    if output.status.success() {
        return Ok(ContinueOutcome::Done);
    }
    let combined = combine_output(&output.stdout, &output.stderr);
    if looks_like_rebase_conflict(&combined)
        || !list_unmerged_paths(cwd).unwrap_or_default().is_empty()
    {
        Ok(ContinueOutcome::Conflict(combined))
    } else {
        Ok(ContinueOutcome::Failed(combined))
    }
}

fn spawn_conflict_agent(
    cwd: &Path,
    cfg: &crate::config::Config,
    client: &DetectedClient,
    model: &str,
    wall: Duration,
    files: &[String],
    context: &str,
) -> Result<()> {
    let prompt = build_conflict_prompt(files, context);
    let label = "parley-rebase-conflict";
    let term = resolve_terminal(cfg.headless, &client.client, "parley");
    if let Some(ref ctx) = term {
        let _guard = AgentPaneCleanupGuard::default();
        preflight_zellij_agent_panes(Some(ctx), "parley", 1)?;
        let sentinel = run_nonheadless(client, model, cwd, &prompt, label, ctx)?;
        let missing = wait_for_sentinels(&[sentinel], wall);
        if !missing.is_empty() {
            eprintln!(
                "scrutiny parley stack: warn: conflict agent pane did not finish within {}s",
                wall.as_secs()
            );
        }
        force_close_agent_panes();
    } else {
        let out = run_headless(
            client,
            model,
            cwd,
            &prompt,
            HeadlessKind::Parley,
            label,
            wall,
        )?;
        if out.code != 0 && !out.timed_out {
            eprintln!(
                "scrutiny parley stack: warn: conflict agent exit {} — checking files anyway",
                out.code
            );
        }
    }
    Ok(())
}

fn build_conflict_prompt(files: &[String], context: &str) -> String {
    let list = if files.is_empty() {
        "(run `git diff --name-only --diff-filter=U` to list them)".into()
    } else {
        files
            .iter()
            .map(|f| format!("  - {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "You are resolving a git rebase conflict during `scrutiny parley stack`.\n\
         Context: {context}\n\n\
         Conflicted files:\n{list}\n\n\
         Rules:\n\
         1. Open each conflicted file. Find markers `<<<<<<<`, `=======`, `>>>>>>>`.\n\
         2. Produce a correct merged result that keeps intentional changes from BOTH sides \
         when possible (this is a stacked PR rebase — do not casually drop either side).\n\
         3. Remove ALL conflict markers. File must compile / parse when it is source code.\n\
         4. `git add -- <file>` for every resolved path.\n\
         5. Do NOT run `git rebase --continue`, `gh stack rebase --continue`, \
         `git rebase --abort`, commit, or push — the host does continue.\n\
         6. Do NOT leave the rebase half-resolved. Stage every previously unmerged path.\n\
         7. Prefer `git status` / `git diff --name-only --diff-filter=U` to verify nothing left.\n\
         When done, stop. Host will continue the rebase.\n"
    )
}

fn open_layers(view: &GhStackView) -> Vec<StackLayer> {
    let mut layers = Vec::new();
    for (i, branch) in view.branches.iter().enumerate() {
        let open = !branch.is_merged
            && branch
                .pr
                .as_ref()
                .map(|p| p.state.eq_ignore_ascii_case("OPEN"))
                .unwrap_or(false);
        if !open {
            continue;
        }
        let pr_number = branch.pr.as_ref().unwrap().number;
        let parent = if i == 0 {
            view.trunk.clone()
        } else {
            view.branches[i - 1].name.clone()
        };
        layers.push(StackLayer {
            name: branch.name.clone(),
            pr_number,
            parent,
        });
    }
    layers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_layers_bottom_to_top_parents() {
        let view: GhStackView = serde_json::from_str(
            r#"{
              "trunk": "main",
              "currentBranch": "feat/02",
              "branches": [
                {
                  "name": "feat/01",
                  "head": "bbb",
                  "base": "aaa",
                  "isMerged": false,
                  "pr": { "number": 10, "url": "https://x/10", "state": "OPEN" }
                },
                {
                  "name": "feat/02",
                  "head": "ccc",
                  "base": "bbb",
                  "isMerged": false,
                  "pr": { "number": 11, "url": "https://x/11", "state": "OPEN" }
                }
              ]
            }"#,
        )
        .unwrap();
        let layers = open_layers(&view);
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].name, "feat/01");
        assert_eq!(layers[0].parent, "main");
        assert_eq!(layers[0].pr_number, 10);
        assert_eq!(layers[1].name, "feat/02");
        assert_eq!(layers[1].parent, "feat/01");
        assert_eq!(layers[1].pr_number, 11);
    }

    #[test]
    fn open_layers_skips_merged_uses_merged_as_parent() {
        let view: GhStackView = serde_json::from_str(
            r#"{
              "trunk": "main",
              "currentBranch": "feat/02",
              "branches": [
                {
                  "name": "feat/01",
                  "isMerged": true,
                  "pr": { "number": 10, "url": "https://x/10", "state": "MERGED" }
                },
                {
                  "name": "feat/02",
                  "isMerged": false,
                  "pr": { "number": 11, "url": "https://x/11", "state": "OPEN" }
                }
              ]
            }"#,
        )
        .unwrap();
        let layers = open_layers(&view);
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].name, "feat/02");
        assert_eq!(layers[0].parent, "feat/01");
    }

    #[test]
    fn looks_like_rebase_conflict_detects_gh_stack_output() {
        let sample = r#"
⚠ Rebasing feat/pr3 onto feat/pr2 — conflict
Conflicted files:
  C src/foo.tsx
To resolve:
  4. Continue:  `gh stack rebase --continue`
"#;
        assert!(looks_like_rebase_conflict(sample));
        assert!(!looks_like_rebase_conflict("fatal: not a git repository"));
    }

    #[test]
    fn conflict_prompt_lists_files_and_forbids_continue() {
        let p = build_conflict_prompt(
            &["a.rs".into(), "b.rs".into()],
            "gh stack rebase after PR #1",
        );
        assert!(p.contains("a.rs"));
        assert!(p.contains("b.rs"));
        assert!(p.contains("Do NOT run"));
        assert!(p.contains("rebase --continue"));
    }
}
