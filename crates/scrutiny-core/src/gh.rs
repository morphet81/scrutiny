//! Thin `gh` CLI wrappers (REST + GraphQL).

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

use crate::paths::{temp_artifact_path, write_json_pretty};

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
        .args(["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"])
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
    let output = Command::new("gh")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("gh {}", args.join(" ")))?;
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
    let output = Command::new("gh")
        .args(["api", "graphql", "--input"])
        .arg(&payload_path)
        .current_dir(cwd)
        .output()
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
