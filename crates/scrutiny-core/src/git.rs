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

/// Fail early with a clean message when `cwd` is not inside a git work tree.
pub fn ensure_git_repo(cwd: &Path) -> Result<()> {
    if !git_ok(cwd, &["rev-parse", "--is-inside-work-tree"]) {
        bail!(
            "not a git repository: {}. scrutiny must run inside a git repo.",
            cwd.display()
        );
    }
    Ok(())
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

    // The current branch: never a valid destination for its own PR. The
    // configured upstream is a remote copy of this same branch, so it is
    // deliberately NOT used as a base source.
    let current = git_stdout(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| normalize_ref(s.trim()))
        .unwrap_or_default();

    // 1) open PR base via gh (best-effort)
    if let Some(base) = try_gh_pr_base(root) {
        if let Some(resolved) = resolve_existing_ref(root, &base) {
            return Ok(resolved);
        }
    }

    // 2) BASE_BRANCH env (project convention)
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

    // 3) fork-point / merge-base against candidates (and origin/*)
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
        // Never pick the current branch as its own base.
        if !current.is_empty() && normalize_ref(&resolved) == current {
            continue;
        }
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

/// Uncommitted changes present?
pub fn is_dirty(root: &Path) -> bool {
    git_stdout(root, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Commits on HEAD not on `base` (0 if unknown).
pub fn commits_ahead(root: &Path, base: &str) -> usize {
    commit_count(root, &format!("{base}..HEAD")).unwrap_or(0)
}

/// Is `branch` one of the configured base candidates (prefix-normalized)?
pub fn is_base_branch(branch: &str, candidates: &[String]) -> bool {
    let b = normalize_ref(branch);
    candidates.iter().any(|c| normalize_ref(c) == b)
}

/// Create + switch to `name` (switch to it if it already exists).
pub fn create_branch(root: &Path, name: &str) -> Result<()> {
    if ref_exists(root, name) {
        if git_ok(root, &["switch", name]) || git_ok(root, &["checkout", name]) {
            return Ok(());
        }
        bail!("could not switch to existing branch {name}");
    }
    if git_ok(root, &["switch", "-c", name]) || git_ok(root, &["checkout", "-b", name]) {
        return Ok(());
    }
    bail!("could not create branch {name}");
}

/// Add a git worktree at `dir` on branch `name` (create the branch unless it exists).
pub fn create_worktree(root: &Path, name: &str, dir: &Path) -> Result<PathBuf> {
    let dir_str = dir.to_string_lossy().to_string();
    let ok = if ref_exists(root, name) {
        git_ok(root, &["worktree", "add", &dir_str, name])
    } else {
        git_ok(root, &["worktree", "add", &dir_str, "-b", name])
    };
    if !ok {
        bail!("git worktree add failed for {} ({name})", dir.display());
    }
    Ok(dir.to_path_buf())
}

/// Remove the worktree at `dir` (force, to drop any uncommitted files).
pub fn remove_worktree(root: &Path, dir: &Path) -> Result<()> {
    let dir_str = dir.to_string_lossy().to_string();
    if !git_ok(root, &["worktree", "remove", "--force", &dir_str]) {
        bail!("git worktree remove failed for {}", dir.display());
    }
    Ok(())
}

/// Delete branch `name` (force). Not an error if it does not exist.
pub fn delete_branch(root: &Path, name: &str) -> Result<()> {
    if !ref_exists(root, name) {
        return Ok(());
    }
    if !git_ok(root, &["branch", "-D", name]) {
        bail!("git branch -D {name} failed");
    }
    Ok(())
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

/// Fetch the PR's base-branch tip and head commit into dedicated refs and
/// return their resolved OIDs `(base_oid, head_oid)`.
///
/// Fetches from `repo_url` (the PR's own repository) so the diff is always
/// scoped to the PR — never to whatever the local `origin` or local branches
/// happen to point at. Writes only `refs/scrutiny/{base,head}`, leaving the
/// user's branches untouched. Hard-fails if the refs cannot be fetched.
pub fn fetch_pr_diff_refs(
    root: &Path,
    repo_url: &str,
    base_branch: &str,
    pr_number: &str,
) -> Result<(String, String)> {
    let base_spec = format!("+refs/heads/{base_branch}:refs/scrutiny/base");
    let head_spec = format!("+refs/pull/{pr_number}/head:refs/scrutiny/head");
    let out = Command::new("git")
        .args(["fetch", "--no-tags", repo_url, &base_spec, &head_spec])
        .current_dir(root)
        .output()
        .with_context(|| format!("git fetch PR refs from {repo_url}"))?;
    if !out.status.success() {
        bail!(
            "could not fetch PR base '{base_branch}' and head #{pr_number} from {repo_url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let base_oid = git_stdout(root, &["rev-parse", "refs/scrutiny/base"])?
        .trim()
        .to_string();
    let head_oid = git_stdout(root, &["rev-parse", "refs/scrutiny/head"])?
        .trim()
        .to_string();
    Ok((base_oid, head_oid))
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

    #[test]
    fn is_base_branch_matches_candidates() {
        let cands = vec!["main".to_string(), "develop".to_string()];
        assert!(is_base_branch("main", &cands));
        assert!(is_base_branch("origin/develop", &cands));
        assert!(!is_base_branch("feat/x", &cands));
    }

    #[test]
    fn branch_dirty_ahead_on_temp_repo() {
        let dir = std::env::temp_dir().join(format!("git-helpers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&dir)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?} failed"
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t.t"]);
        run(&["config", "user.name", "t"]);
        run(&["checkout", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        assert!(is_dirty(&dir));
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
        assert!(!is_dirty(&dir));

        create_branch(&dir, "feat/x-y").unwrap();
        let cur = discover_repo(&dir).unwrap().branch;
        assert_eq!(cur, "feat/x-y");
        std::fs::write(dir.join("b.txt"), "2").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "second"]);
        assert_eq!(commits_ahead(&dir, "main"), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn base_is_parent_not_self_upstream() {
        let dir = std::env::temp_dir().join(format!("git-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&dir)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?} failed"
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t.t"]);
        run(&["config", "user.name", "t"]);
        run(&["checkout", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);

        // Feature branch off main, with a commit.
        create_branch(&dir, "feat/z").unwrap();
        std::fs::write(dir.join("b.txt"), "2").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "feat"]);

        // Even with the current branch among the candidates, the base must be
        // the parent (`main`), never the branch itself.
        let cands = vec!["feat/z".to_string(), "main".to_string()];
        let base = resolve_base_branch(&dir, &cands, None).unwrap();
        assert_eq!(normalize_ref(&base), "main");

        std::fs::remove_dir_all(&dir).ok();
    }
}
