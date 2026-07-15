//! Fetch unresolved PR review threads via GraphQL.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::gh::{ensure_gh, gh_graphql, repo_name_with_owner, split_repo};
use crate::paths::{artifact_path, prepare_artifacts, write_json_pretty};

const THREADS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      url
      reviewThreads(first: 50, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          startLine
          diffSide
          comments(first: 50) {
            nodes {
              id
              databaseId
              body
              url
              author { login }
              path
              line
              createdAt
            }
          }
        }
      }
    }
  }
}
"#;

#[derive(Debug, Clone)]
pub struct ParleyFetchInput {
    pub cwd: PathBuf,
    pub pr: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParleyThreadComment {
    pub id: String,
    #[serde(default)]
    pub database_id: Option<u64>,
    pub body: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub diff_side: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParleyComment {
    /// GraphQL review thread id (`PRRT_…`) — use for thread replies.
    pub id: String,
    /// First comment node id (debug).
    #[serde(default)]
    pub comment_id: String,
    #[serde(default)]
    pub database_id: Option<u64>,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub start_line: Option<u64>,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub diff_side: Option<String>,
    #[serde(default)]
    pub is_outdated: bool,
    #[serde(default)]
    pub author: String,
    pub body: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub comments: Vec<ParleyThreadComment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParleyCommentsFile {
    #[serde(default = "default_version")]
    pub version: u32,
    pub pr_number: u64,
    pub pr_url: String,
    pub repo: String,
    pub comments: Vec<ParleyComment>,
}

fn default_version() -> u32 {
    1
}

/// Resolve PR number + optional URL via `gh pr view`.
pub fn resolve_pr(cwd: &Path, pr: Option<&str>) -> Result<(u64, String)> {
    let mut args = vec![
        "pr".into(),
        "view".into(),
        "--json".into(),
        "number,url".into(),
    ];
    if let Some(pr) = pr {
        args.insert(2, pr.to_string());
    }
    let output = Command::new("gh")
        .args(&args)
        .current_dir(cwd)
        .output()
        .context("gh pr view")?;
    if !output.status.success() {
        bail!(
            "gh pr view failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let v: Value = serde_json::from_slice(&output.stdout).context("parse gh pr view")?;
    let number = v
        .get("number")
        .and_then(|n| n.as_u64())
        .context("pr number missing")?;
    let url = v
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    Ok((number, url))
}

pub fn run_parley_fetch(input: ParleyFetchInput) -> Result<(ParleyCommentsFile, PathBuf)> {
    ensure_gh()?;
    let (pr_number, pr_url) = resolve_pr(&input.cwd, input.pr.as_deref())?;
    prepare_artifacts(&input.cwd, Some(&pr_number.to_string()), &[])?;

    let repo = repo_name_with_owner(&input.cwd)?;
    let (owner, name) = split_repo(&repo)?;

    let mut comments = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let vars = json!({
            "owner": owner,
            "name": name,
            "number": pr_number as i64,
            "cursor": cursor,
        });
        let data = gh_graphql(&input.cwd, THREADS_QUERY, &vars)?;
        let threads = data
            .pointer("/repository/pullRequest/reviewThreads")
            .cloned()
            .unwrap_or(Value::Null);
        let nodes = threads
            .get("nodes")
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default();
        for node in nodes {
            if node.get("isResolved").and_then(|v| v.as_bool()) == Some(true) {
                continue;
            }
            if let Some(c) = parse_thread(&node) {
                comments.push(c);
            }
        }
        let has_next = threads
            .pointer("/pageInfo/hasNextPage")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !has_next {
            break;
        }
        cursor = threads
            .pointer("/pageInfo/endCursor")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if cursor.is_none() {
            break;
        }
    }

    let file = ParleyCommentsFile {
        version: 1,
        pr_number,
        pr_url,
        repo,
        comments,
    };
    let path = artifact_path("parley-comments");
    write_json_pretty(&path, &file)?;
    Ok((file, path))
}

fn parse_thread(node: &Value) -> Option<ParleyComment> {
    let id = node.get("id")?.as_str()?.to_string();
    let path = node
        .get("path")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    let line = node.get("line").and_then(|l| l.as_u64());
    let start_line = node.get("startLine").and_then(|l| l.as_u64());
    let diff_side = node
        .get("diffSide")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    let is_outdated = node
        .get("isOutdated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let comment_nodes = node
        .pointer("/comments/nodes")
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();
    let mut trail = Vec::new();
    for c in &comment_nodes {
        let cid = c.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
        if cid.is_empty() {
            continue;
        }
        trail.push(ParleyThreadComment {
            id: cid,
            database_id: c.get("databaseId").and_then(|d| d.as_u64()),
            body: c.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string(),
            url: c.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string(),
            author: c
                .pointer("/author/login")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string(),
            path: c.get("path").and_then(|p| p.as_str()).map(|s| s.to_string()),
            line: c.get("line").and_then(|l| l.as_u64()),
            // diffSide lives on the thread, not PullRequestReviewComment
            diff_side: diff_side.clone(),
            created_at: c
                .get("createdAt")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
        });
    }
    let first = trail.first();
    let body = first.map(|c| c.body.clone()).unwrap_or_default();
    if body.is_empty() && path.is_empty() {
        return None;
    }
    let author = first.map(|c| c.author.clone()).unwrap_or_default();
    let url = first.map(|c| c.url.clone()).unwrap_or_default();
    let comment_id = first.map(|c| c.id.clone()).unwrap_or_default();
    let database_id = first.and_then(|c| c.database_id);
    let path = if path.is_empty() {
        first
            .and_then(|c| c.path.clone())
            .unwrap_or_default()
    } else {
        path
    };
    let line = line.or_else(|| first.and_then(|c| c.line));
    let side = diff_side.clone();

    Some(ParleyComment {
        id,
        comment_id,
        database_id,
        path,
        line,
        start_line,
        side: side.clone(),
        diff_side: side,
        is_outdated,
        author,
        body,
        url,
        comments: trail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unresolved_thread() {
        let node = json!({
            "id": "PRRT_test",
            "isResolved": false,
            "isOutdated": false,
            "path": "src/foo.rs",
            "line": 42,
            "startLine": null,
            "diffSide": "RIGHT",
            "comments": {
                "nodes": [{
                    "id": "PRRC_1",
                    "databaseId": 99,
                    "body": "Please rename this",
                    "url": "https://example.com",
                    "author": { "login": "reviewer" },
                    "path": "src/foo.rs",
                    "line": 42,
                    "diffSide": "RIGHT",
                    "createdAt": "2026-01-01T00:00:00Z"
                }]
            }
        });
        let c = parse_thread(&node).expect("comment");
        assert_eq!(c.id, "PRRT_test");
        assert_eq!(c.path, "src/foo.rs");
        assert_eq!(c.line, Some(42));
        assert_eq!(c.body, "Please rename this");
        assert_eq!(c.author, "reviewer");
        assert_eq!(c.comment_id, "PRRC_1");
    }

    #[test]
    fn skip_empty_body_no_path() {
        let node = json!({
            "id": "PRRT_empty",
            "isResolved": false,
            "comments": { "nodes": [] }
        });
        assert!(parse_thread(&node).is_none());
    }
}
