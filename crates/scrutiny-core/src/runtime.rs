//! Detect local headless agent CLIs and resolve which client to use.

use anyhow::{bail, Context, Result};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::Config;

#[derive(Debug, Clone)]
pub struct DetectedClient {
    pub client: String,
    pub binary: PathBuf,
}

/// Probe PATH for supported agent CLIs.
pub fn detect_clients() -> Vec<DetectedClient> {
    let mut out = Vec::new();
    if let Some(bin) = first_working(&["agent", "cursor-agent"]) {
        out.push(DetectedClient {
            client: "cursor".into(),
            binary: bin,
        });
    }
    if let Some(bin) = first_working(&["claude"]) {
        out.push(DetectedClient {
            client: "claude".into(),
            binary: bin,
        });
    }
    if let Some(bin) = first_working(&["codex"]) {
        out.push(DetectedClient {
            client: "codex".into(),
            binary: bin,
        });
    }
    out
}

fn first_working(names: &[&str]) -> Option<PathBuf> {
    for name in names {
        if let Ok(path) = which(name) {
            if version_ok(&path) {
                return Some(path);
            }
        }
    }
    None
}

fn which(name: &str) -> Result<PathBuf> {
    let output = Command::new("which")
        .arg(name)
        .output()
        .with_context(|| format!("which {name}"))?;
    if !output.status.success() {
        bail!("not found");
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        bail!("empty");
    }
    Ok(PathBuf::from(s))
}

fn version_ok(bin: &PathBuf) -> bool {
    let mut child = match Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success() || status.code().is_some(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                return false;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolveClientInput {
    pub cli_override: Option<String>,
    pub skip_prompt: bool,
}

/// Resolve client + binary path from config / CLI / detection / prompt.
pub fn resolve_client(cfg: &Config, input: ResolveClientInput) -> Result<DetectedClient> {
    let detected = detect_clients();
    if detected.is_empty() {
        bail!(
            "no headless agent CLI found on PATH (need agent|cursor-agent, claude, and/or codex).\n\
             Install Cursor Agent CLI, Claude Code, or Codex, then retry."
        );
    }

    let forced = input
        .cli_override
        .as_deref()
        .or(cfg.force_client.as_deref())
        .map(|s| s.trim().to_ascii_lowercase());

    if let Some(want) = forced {
        if let Some(d) = detected.iter().find(|d| d.client == want) {
            eprintln!(
                "scrutiny: using client={} ({})",
                d.client,
                d.binary.display()
            );
            return Ok(d.clone());
        }
        let have: Vec<_> = detected.iter().map(|d| d.client.as_str()).collect();
        bail!(
            "forced client '{want}' not available. Detected: {}. Install it or clear force_client / --client.",
            have.join(", ")
        );
    }

    if detected.len() == 1 {
        let d = &detected[0];
        eprintln!(
            "scrutiny: only one agent CLI found — using {} ({})",
            d.client,
            d.binary.display()
        );
        return Ok(d.clone());
    }

    if input.skip_prompt {
        if let Some(d) = detected
            .iter()
            .find(|d| d.client == cfg.default_client)
            .or_else(|| detected.first())
        {
            eprintln!(
                "scrutiny: non-interactive — using {} ({})",
                d.client,
                d.binary.display()
            );
            return Ok(d.clone());
        }
    }

    use dialoguer::{theme::ColorfulTheme, Select};
    use std::io::IsTerminal;

    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        let labels: Vec<String> = detected
            .iter()
            .map(|d| {
                let mark = if d.client == cfg.default_client {
                    "  (default)"
                } else {
                    ""
                };
                format!("{} ({}){mark}", d.client, d.binary.display())
            })
            .collect();
        let default_idx = detected
            .iter()
            .position(|d| d.client == cfg.default_client)
            .unwrap_or(0);
        let sel = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Choose agent CLI")
            .items(&labels)
            .default(default_idx)
            .interact()
            .context("client menu")?;
        return Ok(detected[sel].clone());
    }

    eprintln!("scrutiny: available agent CLIs:");
    for (i, d) in detected.iter().enumerate() {
        let mark = if d.client == cfg.default_client {
            "  <- default"
        } else {
            ""
        };
        eprintln!(
            "  [{}] {} ({}){mark}",
            i + 1,
            d.client,
            d.binary.display()
        );
    }
    eprint!(
        "Choose client 1-{} [default {}]: ",
        detected.len(),
        cfg.default_client
    );
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("read client choice")?;
    let line = line.trim();
    if line.is_empty() {
        if let Some(d) = detected.iter().find(|d| d.client == cfg.default_client) {
            return Ok(d.clone());
        }
        return Ok(detected[0].clone());
    }
    if let Ok(n) = line.parse::<usize>() {
        if n >= 1 && n <= detected.len() {
            return Ok(detected[n - 1].clone());
        }
        bail!("client number out of range");
    }
    let want = line.to_ascii_lowercase();
    detected
        .into_iter()
        .find(|d| d.client == want)
        .ok_or_else(|| anyhow::anyhow!("unknown client '{want}'"))
}

/// Resolve spawn mode: team (default) or isolated.
pub fn resolve_spawn_mode(
    cfg: &Config,
    cli_override: Option<&str>,
    skip_prompt: bool,
) -> Result<String> {
    if let Some(m) = cli_override {
        return normalize_spawn_mode(m);
    }
    if let Some(m) = &cfg.force_spawn_mode {
        return normalize_spawn_mode(m);
    }
    if skip_prompt {
        return Ok("team".into());
    }

    use dialoguer::{theme::ColorfulTheme, Select};
    use std::io::IsTerminal;

    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        let items = [
            "team — one lead agent spawns its own team and builds the report",
            "isolated — script runs reviewers/evangelists/specialists in parallel",
        ];
        let sel = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Spawn mode")
            .items(&items)
            .default(0)
            .interact()
            .context("spawn mode menu")?;
        return Ok(if sel == 0 {
            "team".into()
        } else {
            "isolated".into()
        });
    }

    eprintln!("scrutiny: spawn mode");
    eprintln!("  [1] team     — one lead agent spawns its own team and builds the report (default)");
    eprintln!("  [2] isolated — script runs reviewers/evangelists/specialists in parallel");
    eprint!("Choose 1 or 2 [default 1]: ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("read spawn mode")?;
    let line = line.trim();
    if line.is_empty() || line == "1" || line.eq_ignore_ascii_case("team") {
        return Ok("team".into());
    }
    if line == "2" || line.eq_ignore_ascii_case("isolated") {
        return Ok("isolated".into());
    }
    bail!("expected 1/2/isolated/team, got {line}");
}

pub fn normalize_spawn_mode(raw: &str) -> Result<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "isolated" => Ok("isolated".into()),
        "team" | "full" | "orchestrated" => Ok("team".into()),
        other => bail!("spawn_mode must be isolated|team, got {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_spawn() {
        assert_eq!(normalize_spawn_mode("isolated").unwrap(), "isolated");
        assert_eq!(normalize_spawn_mode("TEAM").unwrap(), "team");
        assert_eq!(normalize_spawn_mode("full").unwrap(), "team");
    }
}
