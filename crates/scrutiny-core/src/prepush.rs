//! Quiet pre-push check gate shared by `parley` and `forge`.
//!
//! Scrutiny runs the repo's pre-push checks itself — quietly, capturing all
//! output to a dedicated log file rather than streaming it to the terminal (a
//! flood of hook output corrupts multiplexer panes; see `spinner.rs`). A cheap
//! plan agent splits the log into scoped chunks; one fix agent per chunk works
//! *only* from that chunk. Agents must not run the checks themselves — scrutiny
//! re-runs them to verify.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::git::git_stdout;

/// One line injected into implementing-agent briefs so each agent owns making
/// its own changes pass the repo's pre-push checks.
pub const PREPUSH_OWNERSHIP: &str =
    "Your changes must pass the repo's pre-push checks (lint / tests / typecheck). \
     Make every file you touch pass them before you finish.\n";

/// One line telling agents not to strew build/test artifacts across the repo.
/// scrutiny excludes known artifact globs from commits, but agents inventing new
/// output dirs defeats that — keep coverage in the repo's own gitignored dir.
pub const NO_ARTIFACTS: &str =
    "Do NOT run coverage/tests into custom output directories. Use the repo's own \
     scripts (e.g. `npm run test:coverage` writes to the gitignored `coverage/`). \
     Leave no build or coverage artifacts behind.\n";

/// Max characters kept in a fallback / truncated excerpt.
pub const EXCERPT_MAX_CHARS: usize = 8_000;

/// Outcome of one quiet check run.
pub struct PrepushResult {
    pub ok: bool,
    pub exit_code: i32,
    pub log_path: PathBuf,
}

/// One scoped fix unit produced by the plan agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrepushChunk {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub excerpt: String,
}

/// Plan-agent output: list of fix chunks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrepushChunksFile {
    pub chunks: Vec<PrepushChunk>,
}

/// Resolve the shell command that runs the repo's pre-push checks.
///
/// - `override_cmd` (config) wins when non-empty.
/// - Otherwise, if a pre-push hook exists, run it the way git would
///   (`git hook run pre-push`, which honors husky's `core.hooksPath`).
/// - Otherwise `None` — no checks to run; the gate is a no-op green.
pub fn resolve_prepush_command(cwd: &Path, override_cmd: Option<&str>) -> Option<String> {
    if let Some(c) = override_cmd {
        let c = c.trim();
        if !c.is_empty() {
            return Some(c.to_string());
        }
    }
    if prepush_hook_exists(cwd) {
        return Some("git hook run pre-push".to_string());
    }
    None
}

/// True when the repo has an executable-ish pre-push hook (honors `core.hooksPath`).
fn prepush_hook_exists(cwd: &Path) -> bool {
    let hooks_dir = match git_stdout(cwd, &["config", "--get", "core.hooksPath"]) {
        Ok(s) if !s.trim().is_empty() => PathBuf::from(s.trim()),
        _ => match git_stdout(cwd, &["rev-parse", "--git-path", "hooks"]) {
            Ok(s) if !s.trim().is_empty() => PathBuf::from(s.trim()),
            _ => return false,
        },
    };
    let hook = if hooks_dir.is_absolute() {
        hooks_dir.join("pre-push")
    } else {
        cwd.join(hooks_dir).join("pre-push")
    };
    hook.exists()
}

/// Run `cmd` quietly (no terminal echo), writing combined stdout+stderr to
/// `log_path`. A spinner covers the wait at the call site.
pub fn run_checks_to_log(cwd: &Path, cmd: &str, log_path: &Path) -> std::io::Result<PrepushResult> {
    let (code, out, err) = crate::forge::verify::run_command(cwd, cmd);
    let combined = format!(
        "$ {cmd}\n\n----- stdout -----\n{out}\n----- stderr -----\n{err}\n----- exit {code} -----\n"
    );
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(log_path, combined.as_bytes())?;
    Ok(PrepushResult {
        ok: code == 0,
        exit_code: code,
        log_path: log_path.to_path_buf(),
    })
}

/// Prompt for the plan agent: read the log, emit chunk JSON only (no edits).
pub fn build_prepush_plan_prompt(findings_path: &Path, max_chunks: u32) -> String {
    format!(
        "{}Findings file (READ ONLY — this is your only input): {}\n\n\
         {}\n\
         Group related failures (same file / package / suite). Cap at {max_chunks} chunks.\n\
         Each chunk needs: id (short slug), title, files (paths to edit), excerpt \
         (only the relevant log lines — not the whole log).\n\n\
         Output ONLY valid JSON matching this shape (no markdown prose outside the JSON):\n\
         {{\"chunks\":[{{\"id\":\"…\",\"title\":\"…\",\"files\":[\"…\"],\"excerpt\":\"…\"}}]}}\n\n\
         Do NOT edit files, run checks, lint, tests, build, commit, or push.\n\
         If the log is unparseable, emit one chunk with id/title \"all\" and a truncated tail excerpt.\n",
        crate::caveman::dialect(
            "Repo pre-push checks (lint / tests / typecheck) FAILING. \
             scrutiny already ran them, saved full output to disk.\n\n",
            "The repository's pre-push checks (lint / tests / typecheck) are FAILING. \
             scrutiny already ran them and saved the full output to disk.\n\n",
        ),
        findings_path.display(),
        crate::caveman::dialect(
            "Your job: analyse log, split work into independent fix chunks.",
            "Your job: analyse the log and split the work into independent fix chunks.",
        ),
    )
}

/// Prompt for a scoped fix agent: act only on one chunk; never run checks / commit / push.
pub fn build_prepush_chunk_fix_prompt(findings_path: &Path, chunk: &PrepushChunk) -> String {
    let files = if chunk.files.is_empty() {
        "(infer from excerpt)".to_string()
    } else {
        chunk
            .files
            .iter()
            .map(|f| format!("- `{f}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "{}Chunk id: {}\n\
         Title: {}\n\
         Files to touch (prefer these; do not wander):\n{files}\n\n\
         Failure excerpt (your primary input):\n\
         -----\n{}\n-----\n\n\
         Full findings log (reference only if excerpt is incomplete): {}\n\n\
         Fix ONLY this chunk's failures. Do NOT weaken, skip, or delete tests.\n\
         Do NOT run the checks, lint, tests, build, or any pre-push command yourself — \
         scrutiny re-runs them and verifies your fix.\n\
         Do NOT git commit, git push, or call gh — the host script commits and retries.\n\
         Do NOT fix failures belonging to other chunks.\n",
        crate::caveman::dialect(
            "Repo pre-push checks (lint / tests / typecheck) FAILING. \
             scrutiny already ran them. You own ONE scoped chunk only.\n\n",
            "The repository's pre-push checks (lint / tests / typecheck) are FAILING. \
             scrutiny already ran them. You own ONE scoped chunk only.\n\n",
        ),
        chunk.id,
        chunk.title,
        truncate_chars(&chunk.excerpt, EXCERPT_MAX_CHARS),
        findings_path.display()
    )
}

/// Legacy single-agent prompt (full log). Kept for tests / callers that want the
/// unscoped form; gate prefers [`build_prepush_chunk_fix_prompt`].
pub fn build_prepush_fix_prompt(findings_path: &Path) -> String {
    format!(
        "{}Findings file (read it — this is your only input): {}\n\n\
         Fix ONLY what the findings report as failing. Do NOT weaken, skip, or delete tests.\n\
         Do NOT run the checks, lint, tests, build, or any pre-push command yourself — \
         scrutiny re-runs them and verifies your fix.\n\
         Do NOT git commit, git push, or call gh — the host script commits and retries.\n",
        crate::caveman::dialect(
            "Repo pre-push checks (lint / tests / typecheck) FAILING. \
             scrutiny already ran them, saved full output to disk.\n\n",
            "The repository's pre-push checks (lint / tests / typecheck) are FAILING. \
             scrutiny already ran them and saved the full output to disk.\n\n",
        ),
        findings_path.display()
    )
}

/// Parse plan-agent stdout into chunks. Empty / invalid → `None`.
pub fn parse_prepush_chunks(raw: &str, max_chunks: u32) -> Option<Vec<PrepushChunk>> {
    let text = extract_json_payload(raw).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    // Claude/Cursor envelope: { "result": "<json string>" } or structured_output.
    if v.get("chunks").is_none() {
        if let Some(r) = v.get("result").and_then(|x| x.as_str()) {
            return parse_prepush_chunks(r, max_chunks);
        }
        if let Some(r) = v.get("structured_output") {
            return parse_prepush_chunks(&r.to_string(), max_chunks);
        }
    }
    let file: PrepushChunksFile = serde_json::from_value(v).ok()?;
    let mut chunks: Vec<PrepushChunk> = file
        .chunks
        .into_iter()
        .filter(|c| !c.id.trim().is_empty() || !c.title.trim().is_empty() || !c.excerpt.is_empty())
        .map(|mut c| {
            if c.id.trim().is_empty() {
                c.id = c.title.clone();
            }
            if c.title.trim().is_empty() {
                c.title = c.id.clone();
            }
            c.excerpt = truncate_chars(&c.excerpt, EXCERPT_MAX_CHARS);
            c
        })
        .collect();
    if chunks.is_empty() {
        return None;
    }
    let cap = max_chunks.max(1) as usize;
    if chunks.len() > cap {
        chunks.truncate(cap);
    }
    Some(chunks)
}

/// Single fallback chunk when the plan agent fails or returns empty JSON.
pub fn fallback_chunk(log_contents: &str) -> PrepushChunk {
    PrepushChunk {
        id: "all".into(),
        title: "all".into(),
        files: Vec::new(),
        excerpt: truncate_chars(log_contents, EXCERPT_MAX_CHARS),
    }
}

/// True when every chunk lists files and no two chunks share a file path.
/// Empty `files` on any chunk → not disjoint (caller should run sequential).
pub fn chunks_files_disjoint(chunks: &[PrepushChunk]) -> bool {
    if chunks.len() <= 1 {
        return true;
    }
    if chunks.iter().any(|c| c.files.is_empty()) {
        return false;
    }
    let mut seen = HashSet::new();
    for c in chunks {
        for f in &c.files {
            let key = f.replace('\\', "/");
            if !seen.insert(key) {
                return false;
            }
        }
    }
    true
}

/// Write chunks JSON to `path` (pretty).
pub fn write_chunks_file(path: &Path, chunks: &[PrepushChunk]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let file = PrepushChunksFile {
        chunks: chunks.to_vec(),
    };
    let body = serde_json::to_string_pretty(&file).context("serialize prepush chunks")?;
    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Pull a JSON object/array out of agent stdout (raw, fenced, or Claude envelope).
fn extract_json_payload(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Ok(trimmed.to_string());
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = after
            .strip_prefix("json")
            .or_else(|| after.strip_prefix("JSON"))
            .unwrap_or(after);
        if let Some(end) = after.find("```") {
            return Ok(after[..end].trim().to_string());
        }
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(r) = v.get("result").and_then(|x| x.as_str()) {
            return extract_json_payload(r);
        }
        return Ok(trimmed.to_string());
    }
    bail!("could not extract JSON from agent stdout");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_over_hook() {
        let cmd = resolve_prepush_command(Path::new("/nonexistent"), Some("npm run verify"));
        assert_eq!(cmd.as_deref(), Some("npm run verify"));
    }

    #[test]
    fn blank_override_is_ignored() {
        let cmd = resolve_prepush_command(Path::new("/nonexistent"), Some("   "));
        assert!(cmd.is_none());
    }

    #[test]
    fn fix_prompt_forbids_running_checks() {
        let p = build_prepush_fix_prompt(Path::new("/tmp/prepush-check-1.log"));
        assert!(p.contains("Do NOT run the checks"));
        assert!(p.contains("/tmp/prepush-check-1.log"));
        assert!(p.contains("Do NOT git commit"));
    }

    #[test]
    fn plan_prompt_asks_for_json_chunks() {
        let p = build_prepush_plan_prompt(Path::new("/tmp/prepush-check-1.log"), 8);
        assert!(p.contains("Do NOT edit files"));
        assert!(p.contains("Cap at 8 chunks"));
        assert!(p.contains("/tmp/prepush-check-1.log"));
        assert!(p.contains("\"chunks\""));
    }

    #[test]
    fn chunk_fix_prompt_scopes_work() {
        let chunk = PrepushChunk {
            id: "lint-foo".into(),
            title: "eslint in foo.ts".into(),
            files: vec!["src/foo.ts".into()],
            excerpt: "error at foo.ts:1".into(),
        };
        let p = build_prepush_chunk_fix_prompt(Path::new("/tmp/log"), &chunk);
        assert!(p.contains("lint-foo"));
        assert!(p.contains("src/foo.ts"));
        assert!(p.contains("error at foo.ts:1"));
        assert!(p.contains("Do NOT run the checks"));
        assert!(p.contains("Do NOT fix failures belonging to other chunks"));
    }

    #[test]
    fn parse_chunks_from_raw_json() {
        let raw = r#"{"chunks":[{"id":"a","title":"A","files":["a.ts"],"excerpt":"fail a"},{"id":"b","title":"B","files":["b.ts"],"excerpt":"fail b"}]}"#;
        let chunks = parse_prepush_chunks(raw, 8).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].id, "a");
        assert_eq!(chunks[1].files, vec!["b.ts"]);
    }

    #[test]
    fn parse_chunks_respects_max() {
        let raw = r#"{"chunks":[
            {"id":"1","title":"1","files":["1.ts"],"excerpt":"e"},
            {"id":"2","title":"2","files":["2.ts"],"excerpt":"e"},
            {"id":"3","title":"3","files":["3.ts"],"excerpt":"e"}
        ]}"#;
        let chunks = parse_prepush_chunks(raw, 2).unwrap();
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn parse_chunks_from_fenced_and_envelope() {
        let fenced = "here\n```json\n{\"chunks\":[{\"id\":\"x\",\"title\":\"x\",\"files\":[],\"excerpt\":\"e\"}]}\n```\n";
        assert_eq!(parse_prepush_chunks(fenced, 8).unwrap()[0].id, "x");

        let envelope = serde_json::json!({
            "result": "{\"chunks\":[{\"id\":\"y\",\"title\":\"y\",\"files\":[\"y.ts\"],\"excerpt\":\"e\"}]}"
        })
        .to_string();
        assert_eq!(parse_prepush_chunks(&envelope, 8).unwrap()[0].id, "y");
    }

    #[test]
    fn parse_empty_or_junk_is_none() {
        assert!(parse_prepush_chunks("", 8).is_none());
        assert!(parse_prepush_chunks("not json", 8).is_none());
        assert!(parse_prepush_chunks(r#"{"chunks":[]}"#, 8).is_none());
    }

    #[test]
    fn fallback_truncates() {
        let long = "x".repeat(EXCERPT_MAX_CHARS + 50);
        let c = fallback_chunk(&long);
        assert_eq!(c.id, "all");
        assert!(c.excerpt.chars().count() <= EXCERPT_MAX_CHARS);
        assert!(c.excerpt.ends_with('…'));
    }

    #[test]
    fn disjoint_detection() {
        let a = PrepushChunk {
            id: "a".into(),
            title: "a".into(),
            files: vec!["a.ts".into()],
            excerpt: String::new(),
        };
        let b = PrepushChunk {
            id: "b".into(),
            title: "b".into(),
            files: vec!["b.ts".into()],
            excerpt: String::new(),
        };
        let overlap = PrepushChunk {
            id: "c".into(),
            title: "c".into(),
            files: vec!["a.ts".into()],
            excerpt: String::new(),
        };
        let empty = PrepushChunk {
            id: "d".into(),
            title: "d".into(),
            files: vec![],
            excerpt: String::new(),
        };
        assert!(chunks_files_disjoint(&[a.clone(), b.clone()]));
        assert!(!chunks_files_disjoint(&[a.clone(), overlap]));
        assert!(!chunks_files_disjoint(&[a, empty]));
        assert!(chunks_files_disjoint(&[b]));
    }
}
