//! Thin `gh` CLI wrappers (REST + GraphQL).

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use crate::paths::{temp_artifact_path, write_json_pretty};

/// Backoff between `gh` retries. Length also sets the attempt count (+1).
const GH_BACKOFF: [u64; 3] = [1, 3, 8];

/// A `gh` failure worth retrying: GitHub 5xx / throttling / transport blips.
/// Anything else (4xx, bad args, auth) is deterministic — retrying only wastes
/// the reviewer's time.
pub fn is_transient(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    const NEEDLES: [&str; 12] = [
        "http 500",
        "http 502",
        "http 503",
        "http 504",
        "http 429",
        "rate limit",
        "secondary rate",
        "timeout",
        "timed out",
        "connection reset",
        "unexpected eof",
        "tls handshake",
    ];
    NEEDLES.iter().any(|n| s.contains(n))
}

/// Run `gh <args>` (optionally `--input <file>` appended), retrying transient
/// failures with backoff. Returns the last `Output` — success or not — so
/// callers keep their own error/fallback handling.
pub fn gh_output_retry(cwd: &Path, args: &[&str], input: Option<&Path>) -> Result<Output> {
    let attempts = GH_BACKOFF.len() + 1;
    let mut last: Option<Output> = None;

    for attempt in 1..=attempts {
        let mut cmd = Command::new("gh");
        cmd.args(args).current_dir(cwd);
        if let Some(path) = input {
            cmd.arg("--input").arg(path);
        }
        let output = cmd
            .output()
            .with_context(|| format!("gh {}", args.join(" ")))?;
        if output.status.success() {
            return Ok(output);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        last = Some(output);
        if attempt == attempts || !is_transient(&stderr) {
            break;
        }
        let wait = GH_BACKOFF[attempt - 1];
        eprintln!(
            "scrutiny: gh transient failure (attempt {attempt}/{attempts}) — retrying in {wait}s… {}",
            stderr.trim()
        );
        std::thread::sleep(Duration::from_secs(wait));
    }

    Ok(last.expect("at least one gh attempt"))
}

pub fn ensure_gh() -> Result<()> {
    if !command_exists("gh") {
        bail!("gh CLI not found — install GitHub CLI");
    }
    Ok(())
}

pub fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn split_repo(repo: &str) -> Result<(String, String)> {
    let mut parts = repo.split('/');
    let owner = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("");
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        bail!("repo must be owner/name, got {repo}");
    }
    Ok((owner.into(), name.into()))
}

pub fn repo_name_with_owner(cwd: &Path) -> Result<String> {
    let output = Command::new("gh")
        .args([
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "-q",
            ".nameWithOwner",
        ])
        .current_dir(cwd)
        .output()
        .context("gh repo view")?;
    if !output.status.success() {
        bail!(
            "gh repo view failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn gh_json(cwd: &Path, args: &[&str]) -> Result<Value> {
    let output = gh_output_retry(cwd, args, None)?;
    if !output.status.success() {
        bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(stdout.trim()).with_context(|| {
        format!(
            "parse gh {} json: {}",
            args.join(" "),
            stdout.chars().take(200).collect::<String>()
        )
    })
}

pub fn gh_graphql(cwd: &Path, query: &str, variables: &Value) -> Result<Value> {
    let payload = json!({
        "query": query,
        "variables": variables,
    });
    let payload_path = temp_artifact_path("scrutiny", "graphql", "payload");
    write_json_pretty(&payload_path, &payload)?;
    let output = gh_output_retry(cwd, &["api", "graphql"], Some(&payload_path))
        .context("run gh api graphql")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        bail!("gh api graphql failed: {stderr} {stdout}");
    }
    let resp: Value = serde_json::from_str(stdout.trim()).context("parse graphql resp")?;
    if let Some(errors) = resp.get("errors").and_then(|e| e.as_array()) {
        if !errors.is_empty() {
            let msgs: Vec<String> = errors
                .iter()
                .map(|e| {
                    e.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown")
                        .to_string()
                })
                .collect();
            bail!("graphql errors: {}", msgs.join("; "));
        }
    }
    Ok(resp.get("data").cloned().unwrap_or(Value::Null))
}

pub fn ensure_ai_tag(body: &str, tag: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return tag.to_string();
    }
    if trimmed.ends_with(tag) {
        trimmed.to_string()
    } else {
        format!("{trimmed}\n\n{tag}")
    }
}

#[cfg(test)]
mod tests {
    use super::is_transient;

    #[test]
    fn detects_observed_502() {
        assert!(is_transient("gh: HTTP 502"));
        assert!(is_transient(
            "gh: HTTP 502\n<html>\n<head><title>502 Bad Gateway</title></head>"
        ));
    }

    #[test]
    fn detects_throttling_and_transport() {
        assert!(is_transient("You have exceeded a secondary rate limit"));
        assert!(is_transient("gh: HTTP 429"));
        assert!(is_transient(
            "post https://api.github.com: net/http: TLS handshake timeout"
        ));
        assert!(is_transient("read: connection reset by peer"));
    }

    #[test]
    fn ignores_deterministic_failures() {
        assert!(!is_transient("gh: HTTP 404 Not Found"));
        assert!(!is_transient("gh: HTTP 422 Unprocessable Entity"));
        assert!(!is_transient("gh: Bad credentials (HTTP 401)"));
        assert!(!is_transient(
            "pull request review thread must be on a line in the diff"
        ));
    }
}
