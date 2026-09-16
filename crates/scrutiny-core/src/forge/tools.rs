//! PATH checks for forge CLIs with install instructions.

use anyhow::{bail, Context, Result};
use std::process::Command;

pub const ACLI_INSTALL: &str = "https://developer.atlassian.com/cloud/acli/guides/install-acli/";
pub const FCLI_INSTALL: &str = "https://github.com/morphet81/figma-cli";
pub const GH_INSTALL: &str = "https://cli.github.com/";
pub const GLAB_INSTALL: &str = "https://gitlab.com/gitlab-org/cli";

pub fn cmd_on_path(name: &str) -> bool {
    if which_ok(name) {
        return true;
    }
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn which_ok(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Require a CLI; bail with install URL.
pub fn require_cmd(name: &str, install_url: &str) -> Result<()> {
    if cmd_on_path(name) {
        return Ok(());
    }
    bail!(
        "{name} not found on PATH.\n\
         Install: {install_url}\n\
         Then re-run `scrutiny forge`."
    );
}

pub fn require_acli() -> Result<()> {
    require_cmd("acli", ACLI_INSTALL)
}

/// Fail fast when `acli` is missing or Jira is not authenticated.
/// Used by multi-ticket `forge` before any ticket work starts.
pub fn require_acli_jira() -> Result<()> {
    require_acli()?;
    let output = Command::new("acli")
        .args(["jira", "auth", "status"])
        .output()
        .context("run acli jira auth status")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    if acli_jira_auth_ok(output.status.success(), &combined) {
        return Ok(());
    }
    let detail = combined.trim();
    if detail.is_empty() {
        bail!(
            "acli is not authenticated to Jira.\n\
             Run: acli jira auth login\n\
             Then re-run `scrutiny forge`."
        );
    }
    bail!(
        "acli is not authenticated to Jira.\n\
         {detail}\n\
         Run: acli jira auth login\n\
         Then re-run `scrutiny forge`."
    );
}

pub(crate) fn acli_jira_auth_ok(status_success: bool, combined: &str) -> bool {
    if !status_success {
        return false;
    }
    let lower = combined.to_ascii_lowercase();
    lower.contains("authenticated") && !lower.contains("not authenticated")
}

pub fn require_gh() -> Result<()> {
    require_cmd("gh", GH_INSTALL)
}

pub fn require_glab() -> Result<()> {
    require_cmd("glab", GLAB_INSTALL)
}

pub fn require_fcli() -> Result<()> {
    require_cmd("fcli", FCLI_INSTALL)
}

/// Soft check: if missing, print skip line and return false.
pub fn playwright_cli_available() -> bool {
    if cmd_on_path("playwright-cli") {
        return true;
    }
    eprintln!("playwright-cli not installed. Skipping question.");
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acli_jira_auth_ok_accepts_authenticated() {
        assert!(acli_jira_auth_ok(
            true,
            "✓ Authenticated\n  Site: example.atlassian.net\n"
        ));
    }

    #[test]
    fn acli_jira_auth_ok_rejects_not_authenticated() {
        assert!(!acli_jira_auth_ok(true, "✗ Not authenticated"));
        assert!(!acli_jira_auth_ok(false, "✓ Authenticated"));
        assert!(!acli_jira_auth_ok(true, "no login"));
    }
}
