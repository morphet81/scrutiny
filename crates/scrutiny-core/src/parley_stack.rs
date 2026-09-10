//! `scrutiny parley stack` — bottom→top parley over a `gh stack`, defer push.

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm};
use serde::Deserialize;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Command;

use crate::gh::gh_output_retry;
use crate::git::git_stdout;
use crate::parley::pr_has_unresolved_comments;
use crate::parley_cmd::{run_parley, ParleyCmdInput};

pub struct ParleyStackInput {
    pub cwd: PathBuf,
    pub stack_number: Option<u64>,
    pub client: Option<String>,
    pub spawn_mode: Option<String>,
    pub from_json: Option<String>,
    pub skip_agents: bool,
    pub skip_ship: bool,
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
            let stderr = String::from_utf8_lossy(&rebase.stderr);
            let stdout = String::from_utf8_lossy(&rebase.stdout);
            let _ = Command::new("git")
                .args(["rebase", "--abort"])
                .current_dir(cwd)
                .output();
            bail!(
                "parley stack stopped at {} (PR #{}): rebase onto {} failed:\n{}{}",
                layer.name,
                layer.pr_number,
                layer.parent,
                stdout.trim(),
                stderr.trim()
            );
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
            bail!(
                "parley stack stopped at {} (PR #{}): gh stack rebase failed: {}",
                layer.name,
                layer.pr_number,
                String::from_utf8_lossy(&rb.stderr)
            );
        }
    }

    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let do_push = if tty {
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
}
