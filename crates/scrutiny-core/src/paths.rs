//! Artifact paths: `<cwd>/.scrutiny/<session>/<kind>.json`
//! Config still lives in `~/.scrutiny/config.toml` (separate).

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

static ARTIFACT_CTX: Mutex<Option<ArtifactCtx>> = Mutex::new(None);

#[derive(Debug, Clone)]
struct ArtifactCtx {
    cwd: PathBuf,
    /// PR number as decimal string, or `local`, or `forge-<id>`.
    session: String,
}

/// Session folder name under `.scrutiny/`.
pub fn session_name(pr: Option<u64>, forge_id: Option<&str>) -> String {
    if let Some(n) = pr {
        return n.to_string();
    }
    if let Some(id) = forge_id {
        return format!("forge-{}", slug(id));
    }
    "local".into()
}

/// Parse PR number from plain digits or a GitHub pull URL.
pub fn parse_pr_number(raw: &str) -> Option<u64> {
    let t = raw.trim();
    if let Ok(n) = t.parse::<u64>() {
        return Some(n);
    }
    // …/pull/123 or …/pulls/123
    for part in t.split('/') {
        if let Ok(n) = part.parse::<u64>() {
            if n > 0 {
                return Some(n);
            }
        }
    }
    None
}

/// If `path` is under `…/.scrutiny/<session>/…`, return that session segment.
pub fn infer_session_from_path(path: &Path) -> Option<String> {
    let comps: Vec<_> = path
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    for i in 0..comps.len() {
        if comps[i] == ".scrutiny" {
            return comps.get(i + 1).cloned().filter(|s| !s.is_empty());
        }
    }
    None
}

/// Bind artifact writes to `<cwd>/.scrutiny/<session>/`. Call once per CLI invocation.
pub fn init_artifact_ctx(cwd: &Path, session: &str) -> Result<PathBuf> {
    let session = {
        let s = session.trim();
        if s.is_empty() {
            "local".into()
        } else {
            slug(s)
        }
    };
    let root = cwd.join(".scrutiny").join(&session);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create artifact dir {}", root.display()))?;
    *ARTIFACT_CTX.lock().expect("artifact ctx lock") = Some(ArtifactCtx {
        cwd: cwd.to_path_buf(),
        session,
    });
    Ok(root)
}

/// Prepare artifacts: gitignore warning + session dir. `pr` optional; else infer from hint paths.
pub fn prepare_artifacts(
    cwd: &Path,
    pr: Option<&str>,
    hint_paths: &[&Path],
) -> Result<PathBuf> {
    warn_if_scrutiny_unignored(cwd);
    let session = pr
        .and_then(parse_pr_number)
        .map(|n| n.to_string())
        .or_else(|| {
            hint_paths
                .iter()
                .find_map(|p| infer_session_from_path(p))
        })
        .or_else(|| std::env::var("SCRUTINY_SESSION").ok())
        .unwrap_or_else(|| "local".into());
    init_artifact_ctx(cwd, &session)
}

fn resolved_root() -> PathBuf {
    if let Some(ctx) = ARTIFACT_CTX.lock().expect("artifact ctx lock").clone() {
        return ctx.cwd.join(".scrutiny").join(ctx.session);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let session = std::env::var("SCRUTINY_SESSION").unwrap_or_else(|_| "local".into());
    cwd.join(".scrutiny").join(slug(&session))
}

/// Path for a durable pipeline artifact (`eval`, `map`, `findings`, …) → `kind.json`.
pub fn artifact_path(kind: &str) -> PathBuf {
    let root = resolved_root();
    let _ = std::fs::create_dir_all(&root);
    root.join(format!("{}.json", slug_kind(kind)))
}

/// Path with unique suffix (agent prompts, ephemeral gh payloads).
pub fn artifact_path_unique(kind: &str) -> PathBuf {
    let root = resolved_root();
    let _ = std::fs::create_dir_all(&root);
    let ts = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let nonce: u32 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
        .unwrap_or(0);
    root.join(format!(
        "{}-{}-{:08x}.json",
        slug_kind(kind),
        ts,
        nonce
    ))
}

/// Legacy name — writes into the active `.scrutiny/<session>/` tree.
/// Uses a unique filename for ephemeral kinds; stable `{kind}.json` otherwise.
pub fn temp_artifact_path(_repo: &str, _branch: &str, kind: &str) -> PathBuf {
    if is_ephemeral_kind(kind) {
        artifact_path_unique(kind)
    } else {
        artifact_path(kind)
    }
}

fn is_ephemeral_kind(kind: &str) -> bool {
    kind == "prompt"
        || kind == "comment"
        || kind == "body"
        || kind == "event"
        || kind == "payload"
        || kind.starts_with("raw-")
}

fn slug_kind(kind: &str) -> String {
    slug(kind)
}

pub fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

pub fn write_json_pretty(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value).context("serialize json")?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// True if git would ignore `.scrutiny` (or the path is outside a work tree).
pub fn scrutiny_is_gitignored(cwd: &Path) -> bool {
    for candidate in [".scrutiny", ".scrutiny/"] {
        let status = Command::new("git")
            .args(["-C"])
            .arg(cwd)
            .args(["check-ignore", "-q", "--", candidate])
            .status();
        if let Ok(st) = status {
            if st.success() {
                return true;
            }
        }
    }
    // Fallback: scan .gitignore text
    gitignore_mentions_scrutiny(cwd)
}

fn gitignore_mentions_scrutiny(cwd: &Path) -> bool {
    let path = cwd.join(".gitignore");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    text.lines().any(|line| {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            return false;
        }
        matches!(
            t.trim_start_matches('/'),
            ".scrutiny" | ".scrutiny/" | "**/.scrutiny" | "**/.scrutiny/"
        ) || t == ".scrutiny/**"
    })
}

/// Prominent stderr warning when `.scrutiny/` is not ignored. Does not abort.
pub fn warn_if_scrutiny_unignored(cwd: &Path) {
    // No git repo → skip (not a tracked project)
    let git = Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output();
    let inside = matches!(git, Ok(o) if o.status.success());
    if !inside {
        return;
    }
    if scrutiny_is_gitignored(cwd) {
        return;
    }
    eprintln!();
    eprintln!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
    eprintln!("!!  WARNING: `.scrutiny/` is NOT listed in `.gitignore`  !!");
    eprintln!("!!  Add this line so review artifacts are not committed: !!");
    eprintln!("!!                                                      !!");
    eprintln!("!!      .scrutiny/                                      !!");
    eprintln!("!!                                                      !!");
    eprintln!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_pr_from_url() {
        assert_eq!(
            parse_pr_number("https://github.com/o/r/pull/42"),
            Some(42)
        );
        assert_eq!(parse_pr_number("99"), Some(99));
    }

    #[test]
    fn infer_session() {
        let p = PathBuf::from("/repo/.scrutiny/42/eval.json");
        assert_eq!(infer_session_from_path(&p).as_deref(), Some("42"));
        let p2 = PathBuf::from("/repo/.scrutiny/local/map.json");
        assert_eq!(infer_session_from_path(&p2).as_deref(), Some("local"));
    }

    #[test]
    fn artifact_paths_under_session() {
        let dir = tempdir().unwrap();
        let root = init_artifact_ctx(dir.path(), "7").unwrap();
        assert_eq!(root, dir.path().join(".scrutiny/7"));
        let p = artifact_path("eval");
        assert_eq!(p, root.join("eval.json"));
        assert!(p.starts_with(dir.path()));
    }

    #[test]
    fn gitignore_line_detect() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "node_modules/\n.scrutiny/\n").unwrap();
        assert!(gitignore_mentions_scrutiny(dir.path()));
    }
}
