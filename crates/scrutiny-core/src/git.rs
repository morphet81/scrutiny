use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct RepoContext {
    pub root: PathBuf,
    pub branch: String,
    pub repo_slug: String,
}

pub fn discover_repo(cwd: &Path) -> Result<RepoContext> {
    let root = git_stdout(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root.trim());
    let branch = git_stdout(&root, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    let repo_slug = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    Ok(RepoContext {
        root,
        branch,
        repo_slug,
    })
}

pub fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve the base branch this branch was created from.
pub fn resolve_base_branch(
    root: &Path,
    candidates: &[String],
    pr_base: Option<&str>,
) -> Result<String> {
    if let Some(base) = pr_base {
        let resolved = resolve_existing_ref(root, base)
            .ok_or_else(|| anyhow::anyhow!("PR base ref not found locally: {base} (fetch first?)"))?;
        return Ok(resolved);
    }

    // 1) configured upstream
    if let Ok(upstream) = git_stdout(root, &["rev-parse", "--abbrev-ref", "@{upstream}"]) {
        let upstream = upstream.trim().to_string();
        if !upstream.is_empty() && upstream != "HEAD" {
            return Ok(upstream);
        }
    }

    // 2) open PR base via gh (best-effort)
    if let Some(base) = try_gh_pr_base(root) {
        if let Some(resolved) = resolve_existing_ref(root, &base) {
            return Ok(resolved);
        }
    }

    // 3) BASE_BRANCH env (project convention)
    if let Ok(env_base) = std::env::var("BASE_BRANCH") {
        let env_base = env_base.trim().to_string();
        if !env_base.is_empty() {
            if let Some(resolved) = resolve_existing_ref(root, &env_base) {
                if merge_base(root, &resolved).is_some() {
                    return Ok(resolved);
                }
            }
        }
    }

    // 4) fork-point / merge-base against candidates (and origin/*)
    let mut expanded: Vec<String> = Vec::new();
    for cand in candidates {
        expanded.push(cand.clone());
        if !cand.starts_with("origin/") {
            expanded.push(format!("origin/{cand}"));
        }
    }

    let mut best: Option<(String, usize)> = None;
    for cand in &expanded {
        let Some(resolved) = resolve_existing_ref(root, cand) else {
            continue;
        };
        let Some(base_commit) = fork_point(root, &resolved).or_else(|| merge_base(root, &resolved))
        else {
            continue;
        };
        let ahead = commit_count(root, &format!("{base_commit}..HEAD")).unwrap_or(usize::MAX);
        match &best {
            None => best = Some((resolved, ahead)),
            Some((_, best_ahead)) if ahead < *best_ahead => best = Some((resolved, ahead)),
            _ => {}
        }
    }

    if let Some((name, _)) = best {
        return Ok(name);
    }

    bail!(
        "could not resolve base branch; set upstream, open a PR, set BASE_BRANCH, or add candidates in ~/.scrutiny/config.toml"
    );
}

/// Return a ref name that `git rev-parse` accepts, preferring the given name.
fn resolve_existing_ref(root: &Path, name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if git_ok(root, &["rev-parse", "--verify", &format!("{name}^{{commit}}")]) {
        return Some(name.to_string());
    }
    let short = normalize_ref(name);
    if short != name
        && git_ok(
            root,
            &["rev-parse", "--verify", &format!("{short}^{{commit}}")],
        )
    {
        return Some(short);
    }
    let origin = format!("origin/{short}");
    if git_ok(
        root,
        &["rev-parse", "--verify", &format!("{origin}^{{commit}}")],
    ) {
        return Some(origin);
    }
    None
}

fn normalize_ref(r: &str) -> String {
    r.trim()
        .trim_start_matches("refs/heads/")
        .trim_start_matches("origin/")
        .to_string()
}

pub fn ref_exists(root: &Path, name: &str) -> bool {
    git_ok(root, &["rev-parse", "--verify", &format!("{name}^{{commit}}")])
        || git_ok(
            root,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{name}^{{commit}}"),
            ],
        )
        || git_ok(
            root,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/remotes/origin/{name}^{{commit}}"),
            ],
        )
}

fn fork_point(root: &Path, cand: &str) -> Option<String> {
    git_stdout(root, &["merge-base", "--fork-point", cand, "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn merge_base(root: &Path, cand: &str) -> Option<String> {
    git_stdout(root, &["merge-base", cand, "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn commit_count(root: &Path, range: &str) -> Option<usize> {
    git_stdout(root, &["rev-list", "--count", range])
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn try_gh_pr_base(root: &Path) -> Option<String> {
    let out = Command::new("gh")
        .args(["pr", "view", "--json", "baseRefName", "-q", ".baseRefName"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[derive(Debug, Clone)]
pub struct DiffFile {
    pub path: String,
    pub added: u32,
    pub deleted: u32,
    pub status: String,
}

/// `git diff --numstat` for `base...head` (triple-dot).
pub fn diff_numstat(root: &Path, base: &str, head: &str) -> Result<Vec<DiffFile>> {
    let range = format!("{base}...{head}");
    let out = git_stdout(root, &["diff", "--numstat", "--find-renames", &range])?;
    let mut files = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let added = parts[0].parse::<u32>().unwrap_or(0);
        let deleted = parts[1].parse::<u32>().unwrap_or(0);
        let path = parts[2].to_string();
        files.push(DiffFile {
            path,
            added,
            deleted,
            status: "M".into(),
        });
    }

    // Enrich with name-status
    let ns = git_stdout(root, &["diff", "--name-status", "--find-renames", &range])?;
    let mut status_map = std::collections::BTreeMap::new();
    for line in ns.lines() {
        let mut it = line.split('\t');
        let Some(st) = it.next() else { continue };
        let path = it.next().unwrap_or("").to_string();
        // renames: status\told\tnew
        let path = if st.starts_with('R') {
            it.next().unwrap_or(&path).to_string()
        } else {
            path
        };
        status_map.insert(path, st.chars().next().unwrap_or('M').to_string());
    }
    for f in &mut files {
        if let Some(st) = status_map.get(&f.path) {
            f.status = st.clone();
        }
    }
    Ok(files)
}

pub fn diff_unified_paths(root: &Path, base: &str, head: &str, paths: &[String]) -> Result<String> {
    if paths.is_empty() {
        return Ok(String::new());
    }
    let range = format!("{base}...{head}");
    let mut args = vec!["diff", "--unified=3", "--find-renames", &range, "--"];
    let path_refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    args.extend(path_refs);
    git_stdout(root, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_prefixes() {
        assert_eq!(normalize_ref("refs/heads/main"), "main");
        assert_eq!(normalize_ref("origin/develop"), "develop");
    }
}
