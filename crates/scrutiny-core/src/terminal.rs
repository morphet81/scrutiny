//! Detect the terminal surface and launch a visible agent window on it.
//!
//! Used by non-headless parley (`headless = false`): each agent runs in its own
//! visible window/pane in claude auto mode instead of a captured headless child.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalContext {
    Tmux,
    Zellij,
    ITerm2,
    AppleTerminal,
}

/// Detect the active terminal surface from the environment.
///
/// Multiplexer wins over the host emulator: `$TMUX` / `$ZELLIJ` are set even
/// inside iTerm2 or Terminal.app, and that is the surface we can spawn into.
pub fn detect_terminal() -> Option<TerminalContext> {
    detect_from_env(
        std::env::var("TMUX").ok().as_deref(),
        std::env::var("ZELLIJ").ok().as_deref(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
    )
}

/// Pure detection core (testable without touching the process environment).
pub fn detect_from_env(
    tmux: Option<&str>,
    zellij: Option<&str>,
    term_program: Option<&str>,
) -> Option<TerminalContext> {
    if tmux.map(|v| !v.is_empty()).unwrap_or(false) {
        return Some(TerminalContext::Tmux);
    }
    if zellij.map(|v| !v.is_empty()).unwrap_or(false) {
        return Some(TerminalContext::Zellij);
    }
    match term_program {
        Some("iTerm.app") => Some(TerminalContext::ITerm2),
        Some("Apple_Terminal") => Some(TerminalContext::AppleTerminal),
        _ => None,
    }
}

/// Decide the non-headless terminal surface for a spawned agent, or `None` to
/// run headless. `tool` names the caller (parley/probe/forge) for log prefixes.
///
/// `None` when: `headless = true`; the client is not claude (non-headless
/// supports claude only); or no supported surface is detected.
pub fn resolve_terminal(headless: bool, client: &str, tool: &str) -> Option<TerminalContext> {
    if headless {
        return None;
    }
    match detect_terminal() {
        Some(_) if client != "claude" => {
            eprintln!(
                "scrutiny {tool}: headless=false but non-headless mode supports claude only \
                 (got {client}) — running headless"
            );
            None
        }
        Some(t) => {
            eprintln!(
                "scrutiny {tool}: headless=false — opening agents in {t:?} windows (auto mode)"
            );
            Some(t)
        }
        None => {
            eprintln!(
                "scrutiny {tool}: headless=false but no supported terminal surface \
                 (tmux/zellij/iTerm2/Terminal.app) — running headless"
            );
            None
        }
    }
}

/// Open a new visible window/session running `bash <script_path>` on `ctx`.
///
/// Returns once the launcher command exits — the agent keeps running in its own
/// window; the host waits on the agent's completion sentinel, not on this call.
pub fn launch_agent_window(ctx: TerminalContext, label: &str, script_path: &Path) -> Result<()> {
    let script = script_path.display().to_string();
    let run_cmd = format!("bash '{script}'");
    let status = match ctx {
        TerminalContext::Tmux => Command::new("tmux")
            .args(["new-session", "-d", "-s", &tmux_session_name(label), &run_cmd])
            .status()
            .context("spawn tmux new-session")?,
        TerminalContext::Zellij => Command::new("zellij")
            .args(["run", "--name", label, "--", "bash", &script])
            .status()
            .context("spawn zellij run")?,
        TerminalContext::AppleTerminal => Command::new("osascript")
            .args([
                "-e",
                &format!("tell application \"Terminal\" to do script \"{run_cmd}\""),
            ])
            .status()
            .context("spawn Terminal.app window")?,
        TerminalContext::ITerm2 => Command::new("osascript")
            .args(["-e", &iterm2_new_window_script(&run_cmd)])
            .status()
            .context("spawn iTerm2 window")?,
    };
    if !status.success() {
        bail!("terminal launcher for {label} exited with {status}");
    }
    Ok(())
}

/// tmux session names cannot contain `.` or `:`.
fn tmux_session_name(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Open a fresh iTerm2 window and run `cmd` in its session.
fn iterm2_new_window_script(cmd: &str) -> String {
    format!(
        "tell application \"iTerm\"\n\
         \tset w to (create window with default profile)\n\
         \ttell current session of w to write text \"{cmd}\"\n\
         end tell"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiplexer_wins_over_emulator() {
        assert_eq!(
            detect_from_env(Some("/tmp/tmux-501/default,123,0"), None, Some("iTerm.app")),
            Some(TerminalContext::Tmux)
        );
        assert_eq!(
            detect_from_env(None, Some("0"), Some("Apple_Terminal")),
            Some(TerminalContext::Zellij)
        );
    }

    #[test]
    fn emulator_detection() {
        assert_eq!(
            detect_from_env(None, None, Some("iTerm.app")),
            Some(TerminalContext::ITerm2)
        );
        assert_eq!(
            detect_from_env(None, None, Some("Apple_Terminal")),
            Some(TerminalContext::AppleTerminal)
        );
    }

    #[test]
    fn empty_and_unknown_are_none() {
        assert_eq!(detect_from_env(Some(""), Some(""), Some("vscode")), None);
        assert_eq!(detect_from_env(None, None, None), None);
    }

    #[test]
    fn tmux_session_name_sanitized() {
        assert_eq!(tmux_session_name("parley-member#1"), "parley-member-1");
    }

    #[test]
    fn resolve_terminal_headless_is_none() {
        // headless=true short-circuits before any env detection.
        assert_eq!(resolve_terminal(true, "claude", "probe"), None);
    }
}
