//! Jira write ops for `forge-all` (assign + transition via `acli`).

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

use crate::forge::fetch::jira_key_from_url_or_raw;
use crate::forge::tools::require_acli;

/// Self-assign (or assign to `assignee`) a Jira work item.
pub fn jira_assign(cwd: &Path, key_or_url: &str, assignee: &str) -> Result<String> {
    require_acli()?;
    let key = jira_key_from_url_or_raw(key_or_url)?;
    let output = Command::new("acli")
        .args([
            "jira",
            "workitem",
            "assign",
            "--key",
            &key,
            "--assignee",
            assignee,
            "--yes",
        ])
        .current_dir(cwd)
        .output()
        .context("acli jira workitem assign")?;
    if !output.status.success() {
        bail!(
            "acli assign {key} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(key)
}

/// Transition a Jira work item to `status` (e.g. "In Progress").
pub fn jira_transition(cwd: &Path, key_or_url: &str, status: &str) -> Result<String> {
    require_acli()?;
    let key = jira_key_from_url_or_raw(key_or_url)?;
    let output = Command::new("acli")
        .args([
            "jira",
            "workitem",
            "transition",
            "--key",
            &key,
            "--status",
            status,
            "--yes",
        ])
        .current_dir(cwd)
        .output()
        .context("acli jira workitem transition")?;
    if !output.status.success() {
        bail!(
            "acli transition {key} → {status} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(key)
}
