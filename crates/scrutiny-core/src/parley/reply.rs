//! Post thread replies from parley-fixes.json (script only).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::findings::AI_AGENT_TAG;
use crate::gh::{ensure_ai_tag, ensure_gh, gh_graphql};
use crate::parley::fixes::{compose_reply_body, load_fixes, FixEntry};
use crate::paths::{artifact_path, write_json_pretty};

const REPLY_MUTATION: &str = r#"
mutation($input: AddPullRequestReviewThreadReplyInput!) {
  addPullRequestReviewThreadReply(input: $input) {
    comment {
      id
      url
    }
  }
}
"#;

#[derive(Debug, Clone)]
pub struct ParleyReplyInput {
    pub fixes_path: PathBuf,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParleyReplyResult {
    pub version: u32,
    pub posted: u32,
    pub skipped: u32,
    /// Thread ids skipped because they were host stubs (agent produced nothing
    /// real). Surfaced to the user — these threads were NOT answered.
    #[serde(default)]
    pub stubbed: Vec<String>,
    pub failed: Vec<String>,
    pub replies: Vec<PostedReply>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostedReply {
    pub thread_id: String,
    pub comment_url: Option<String>,
}

pub fn run_parley_reply(input: ParleyReplyInput) -> Result<(ParleyReplyResult, PathBuf)> {
    ensure_gh()?;
    let fixes = load_fixes(&input.fixes_path)?;
    let mut posted = 0u32;
    let mut skipped = 0u32;
    let mut stubbed = Vec::new();
    let mut failed = Vec::new();
    let mut replies = Vec::new();

    // Degenerate-output guard: if every postable (non-stub, non-empty) reply is
    // byte-identical, the run collapsed (e.g. all members timed out into the same
    // fallback). Post nothing rather than spam the PR with N copies.
    let postable: Vec<&FixEntry> = fixes
        .fixes
        .iter()
        .filter(|f| !f.stub && !compose_reply_body(f).trim().is_empty())
        .collect();
    if postable.len() > 1 && all_identical_bodies(&postable) {
        bail!(
            "parley-reply: refusing to post — all {} replies are identical ({:?}). \
             This means the run degenerated (agents produced no distinct fixes). \
             Nothing posted; re-run parley.",
            postable.len(),
            compose_reply_body(postable[0]).chars().take(80).collect::<String>()
        );
    }

    for fix in &fixes.fixes {
        // Never post a host stub — it is a failure placeholder, not a reply.
        if fix.stub {
            stubbed.push(fix.comment_id.clone());
            continue;
        }
        let body = compose_reply_body(fix);
        if body.trim().is_empty() {
            skipped += 1;
            continue;
        }
        let body = ensure_ai_tag(&body, AI_AGENT_TAG);
        let vars = json!({
            "input": {
                "pullRequestReviewThreadId": fix.comment_id,
                "body": body,
            }
        });
        match gh_graphql(&input.cwd, REPLY_MUTATION, &vars) {
            Ok(data) => {
                let url = data
                    .pointer("/addPullRequestReviewThreadReply/comment/url")
                    .and_then(|u| u.as_str())
                    .map(|s| s.to_string());
                let id_ok = data
                    .pointer("/addPullRequestReviewThreadReply/comment/id")
                    .and_then(|i| i.as_str())
                    .is_some();
                if id_ok {
                    posted += 1;
                    replies.push(PostedReply {
                        thread_id: fix.comment_id.clone(),
                        comment_url: url,
                    });
                } else {
                    failed.push(format!(
                        "{}: no comment id in response: {data}",
                        fix.comment_id
                    ));
                }
            }
            Err(e) => {
                failed.push(format!("{}: {e:#}", fix.comment_id));
            }
        }
    }

    let result = ParleyReplyResult {
        version: 1,
        posted,
        skipped,
        stubbed,
        failed,
        replies,
    };
    let path = artifact_path("parley-reply");
    write_json_pretty(&path, &result)?;
    if !result.failed.is_empty() {
        eprintln!(
            "scrutiny parley-reply: {} failed",
            result.failed.len()
        );
        for f in &result.failed {
            eprintln!("  - {f}");
        }
    }
    if !result.stubbed.is_empty() {
        eprintln!(
            "⚠ scrutiny parley: {} thread(s) NOT answered — agents produced no fix. Re-run parley:",
            result.stubbed.len()
        );
        for id in &result.stubbed {
            eprintln!("  - {id}");
        }
    }
    Ok((result, path))
}

/// True when every entry composes to the same reply body — a degenerate run.
fn all_identical_bodies(entries: &[&FixEntry]) -> bool {
    let mut iter = entries.iter().map(|f| compose_reply_body(f));
    match iter.next() {
        Some(first) => iter.all(|b| b == first),
        None => false,
    }
}

/// Dry helper for unit tests of mutation variables.
pub fn reply_input_json(thread_id: &str, body: &str) -> serde_json::Value {
    json!({
        "input": {
            "pullRequestReviewThreadId": thread_id,
            "body": body,
        }
    })
}

pub fn load_reply_result(path: &Path) -> Result<ParleyReplyResult> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_payload_uses_thread_id() {
        let v = reply_input_json("PRRT_abc", "hello");
        assert_eq!(
            v["input"]["pullRequestReviewThreadId"].as_str(),
            Some("PRRT_abc")
        );
        assert_eq!(v["input"]["body"].as_str(), Some("hello"));
    }

    #[test]
    fn identical_bodies_detected() {
        let a = FixEntry {
            comment_id: "a".into(),
            reply_body: "same".into(),
            ..Default::default()
        };
        let b = FixEntry {
            comment_id: "b".into(),
            reply_body: "same".into(),
            ..Default::default()
        };
        let c = FixEntry {
            comment_id: "c".into(),
            reply_body: "different".into(),
            ..Default::default()
        };
        assert!(all_identical_bodies(&[&a, &b]));
        assert!(!all_identical_bodies(&[&a, &c]));
    }

    #[test]
    fn empty_body_not_for_graphql() {
        // compose handled in fixes; here we only check helper shape
        let v = reply_input_json("PRRT_x", "");
        assert_eq!(v["input"]["body"].as_str(), Some(""));
    }
}
