//! Wrap `npx skills add` to install scrutiny skills.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct SkillsInstallInput {
    pub cwd: PathBuf,
    pub global: bool,
    pub skill: String,
    pub agent: Option<String>,
    pub yes: bool,
    /// Override source (path or owner/repo). Default: local checkout or morphet81/scrutiny.
    pub source: Option<String>,
}

pub fn run_skills_install(input: SkillsInstallInput) -> Result<()> {
    which_npx()?;

    let source = input
        .source
        .clone()
        .unwrap_or_else(|| resolve_source(&input.cwd));

    let mut args = vec!["skills".into(), "add".into(), source];
    if input.global {
        args.push("-g".into());
    }
    if input.yes {
        args.push("-y".into());
    }
    args.push("--skill".into());
    args.push(input.skill.clone());
    if let Some(agent) = &input.agent {
        args.push("--agent".into());
        args.push(agent.clone());
    }

    eprintln!("scrutiny skills-install: npx {}", args.join(" "));
    let status = Command::new("npx")
        .args(&args)
        .current_dir(&input.cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("run npx skills add")?;

    if !status.success() {
        bail!(
            "npx skills add failed with exit {}",
            status.code().unwrap_or(1)
        );
    }
    Ok(())
}

fn which_npx() -> Result<()> {
    let output = Command::new("which").arg("npx").output()?;
    if !output.status.success() {
        bail!("npx not found on PATH — install Node.js / npm first");
    }
    Ok(())
}

fn resolve_source(cwd: &Path) -> String {
    if let Some(root) = find_scrutiny_checkout(cwd) {
        return root.display().to_string();
    }
    std::env::var("SCRUTINY_GITHUB_REPO").unwrap_or_else(|_| "morphet81/scrutiny".into())
}

fn find_scrutiny_checkout(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    for _ in 0..12 {
        let scrutiny = cur.join("skills/scrutiny/SKILL.md");
        let forge = cur.join("skills/forge/SKILL.md");
        let cargo = cur.join("Cargo.toml");
        if scrutiny.exists() && forge.exists() && cargo.exists() {
            return Some(cur);
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_repo_from_crate() {
        let start = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root = find_scrutiny_checkout(&start).expect("find checkout");
        assert!(root.join("skills/scrutiny/SKILL.md").exists());
    }
}
