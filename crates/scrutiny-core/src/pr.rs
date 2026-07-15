//! Shared PR-metadata UX: suggest → confirm title inline → optionally edit the
//! description in the user's editor → confirm base branch → `gh pr create`.
//! Used by both `scrutiny pr` (standalone) and `scrutiny forge` (ship step).

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input};
use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;

use crate::config::Config;
use crate::git::{self, git_ok};

/// Editor for PR descriptions: config `editor` → `$VISUAL` → `$EDITOR` → `vi`.
pub fn resolve_editor(cfg: &Config) -> String {
    pick_editor(
        cfg.editor.as_deref(),
        std::env::var("VISUAL").ok().as_deref(),
        std::env::var("EDITOR").ok().as_deref(),
    )
}

/// Pure editor-precedence core (testable without touching the environment).
fn pick_editor(cfg: Option<&str>, visual: Option<&str>, editor: Option<&str>) -> String {
    [cfg, visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("vi")
        .to_string()
}

/// Open `initial` in the preferred editor; return the saved contents (trimmed).
/// The editor command may include args, e.g. `"code --wait"`.
pub fn edit_in_editor(cfg: &Config, dir: &Path, filename: &str, initial: &str) -> Result<String> {
    let path = dir.join(filename);
    fs::write(&path, initial).with_context(|| format!("write {}", path.display()))?;

    let editor = resolve_editor(cfg);
    let mut parts = editor.split_whitespace();
    let prog = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty editor command"))?;
    let status = Command::new(prog)
        .args(parts)
        .arg(&path)
        .current_dir(dir)
        .status()
        .with_context(|| format!("launch editor `{editor}`"))?;
    if !status.success() {
        bail!("editor `{editor}` exited with {status}");
    }

    let edited = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(edited.trim().to_string())
}

/// Confirmed PR metadata.
#[derive(Debug, Clone)]
pub struct PrMetaChoice {
    pub title: String,
    pub body: String,
    pub base: String,
}

/// Compute the default base branch for a PR (resolved, `origin/` stripped).
pub fn default_base_branch(cwd: &Path, candidates: &[String]) -> String {
    let base = git::resolve_base_branch(cwd, candidates, None).unwrap_or_else(|_| "main".into());
    base.strip_prefix("origin/").unwrap_or(&base).to_string()
}

/// Confirm PR title (inline), base branch (inline), and optionally edit the
/// description in the editor. On non-TTY / `skip_prompts`, returns the
/// suggestions and computed base unchanged.
pub fn confirm_pr_meta(
    cfg: &Config,
    cwd: &Path,
    dir: &Path,
    suggested_title: &str,
    suggested_body: &str,
    skip_prompts: bool,
) -> Result<PrMetaChoice> {
    let default_base = default_base_branch(cwd, &cfg.git.base_candidates);
    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();

    if skip_prompts || !tty {
        let title = suggested_title.trim().to_string();
        if title.is_empty() {
            bail!("pr title empty");
        }
        return Ok(PrMetaChoice {
            title,
            body: suggested_body.to_string(),
            base: default_base,
        });
    }

    let title: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("PR title")
        .default(suggested_title.trim().to_string())
        .interact_text()
        .context("pr title")?
        .trim()
        .to_string();
    if title.is_empty() {
        bail!("pr title empty");
    }

    let base: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Base branch")
        .default(default_base)
        .interact_text()
        .context("base branch")?
        .trim()
        .to_string();
    if base.is_empty() {
        bail!("base branch empty");
    }

    let edit = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Edit description in editor?")
        .default(false)
        .interact()
        .context("edit description confirm")?;
    let body = if edit {
        edit_in_editor(cfg, dir, "pr-body.md", suggested_body)?
    } else {
        suggested_body.to_string()
    };

    Ok(PrMetaChoice { title, body, base })
}

/// Push the current branch if it has no upstream, then create the PR via `gh`.
/// Returns the PR URL.
pub fn create_pr(
    cwd: &Path,
    dir: &Path,
    base: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<String> {
    if !git_ok(cwd, &["rev-parse", "--abbrev-ref", "@{upstream}"]) {
        let sp = crate::spinner::Spinner::start(
            "git push -u origin HEAD — running pre-push hooks",
        );
        let push = Command::new("git")
            .args(["push", "-u", "origin", "HEAD"])
            .current_dir(cwd)
            .output()
            .context("git push -u origin HEAD")?;
        if push.status.success() {
            sp.stop_ok("pushed HEAD → origin");
        } else {
            sp.stop_fail("git push failed");
            bail!(
                "git push failed: {}",
                String::from_utf8_lossy(&push.stderr).trim()
            );
        }
    }

    let body_path = dir.join("pr-body.md");
    fs::write(&body_path, body.as_bytes())
        .with_context(|| format!("write {}", body_path.display()))?;

    let mut args = vec!["pr", "create"];
    if draft {
        args.push("--draft");
    }
    args.extend(["--base", base, "--title", title, "--body-file"]);
    let out = Command::new("gh")
        .args(&args)
        .arg(&body_path)
        .current_dir(cwd)
        .output()
        .context("gh pr create")?;
    if !out.status.success() {
        bail!(
            "gh pr create failed: {} {}",
            String::from_utf8_lossy(&out.stderr).trim(),
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_prefers_config_over_env() {
        assert_eq!(pick_editor(Some("nano"), Some("gvim"), Some("vim")), "nano");
    }

    #[test]
    fn editor_visual_over_editor() {
        assert_eq!(pick_editor(None, Some("gvim"), Some("vim")), "gvim");
    }

    #[test]
    fn editor_skips_blank_config() {
        assert_eq!(pick_editor(Some("  "), None, Some("vim")), "vim");
    }

    #[test]
    fn editor_falls_back_to_vi() {
        assert_eq!(pick_editor(None, None, None), "vi");
    }
}
