//! PATH checks for forge CLIs with install instructions.

use anyhow::{bail, Result};
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
