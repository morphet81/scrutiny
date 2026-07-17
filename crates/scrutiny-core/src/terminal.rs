//! Detect the terminal surface and launch a visible agent window on it.
//!
//! Used by non-headless parley (`headless = false`): each agent runs in its own
//! visible window/pane in claude auto mode instead of a captured headless child.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalContext {
    Tmux,
    Zellij,
    ITerm2,
    AppleTerminal,
}

/// A per-item terminal container (one per bulk ticket): agents for that item are
/// launched into it so panes stay grouped. See [`open_item_surface`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ItemSurface {
    /// Detached tmux session named after the item key.
    Tmux { session: String },
    /// Zellij tab named after the item key.
    Zellij { tab: String },
    /// iTerm2 window (AppleScript numeric window id).
    ITerm2 { window_id: String },
    /// Terminal.app window (AppleScript numeric window id).
    Apple { window_id: String },
}

/// Intra-process half of the focus serialization (see [`focus_guard`]).
static TERM_LAUNCH: Mutex<()> = Mutex::new(());

/// Serializes focus-dependent launches (zellij tab focus, Terminal.app) so a
/// pane never lands in the wrong container when items launch concurrently.
///
/// A per-process mutex alone is not enough: bulk spawns the real agents from
/// separate child driver processes, so `TERM_LAUNCH` in one process cannot see
/// the others. We also hold an `flock` on a shared lockfile — cross-process, and
/// auto-released when the fd closes on process death (no stale locks).
struct FocusGuard {
    _proc: std::sync::MutexGuard<'static, ()>,
    #[cfg(unix)]
    _lock: Option<std::fs::File>,
}

fn focus_guard() -> FocusGuard {
    let _proc = TERM_LAUNCH.lock().expect("term launch lock");
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(std::env::temp_dir().join("scrutiny-term-focus.lock"))
            .ok();
        if let Some(f) = &lock {
            unsafe {
                libc::flock(f.as_raw_fd(), libc::LOCK_EX);
            }
        }
        FocusGuard { _proc, _lock: lock }
    }
    #[cfg(not(unix))]
    {
        FocusGuard { _proc }
    }
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
            .args(["run", "--close-on-exit", "--name", label, "--", "bash", &script])
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

// ---- Per-item containers (bulk mode) --------------------------------------

/// Create a per-item container with one idle placeholder pane/tab (cd'd to
/// `cwd`) so it survives `--close-on-exit` agent panes. Returns a handle used by
/// [`launch_agent_in_surface`] to place that item's agents into it.
pub fn open_item_surface(ctx: TerminalContext, key: &str, cwd: &Path) -> Result<ItemSurface> {
    let cwd = cwd.display().to_string();
    match ctx {
        TerminalContext::Tmux => {
            let session = tmux_session_name(key);
            run_argv("tmux", &tmux_open_argv(&session, &cwd)).context("tmux new-session")?;
            // `-c` only sets the initial dir; an interactive login shell profile can
            // `cd` away during startup. Send an explicit cd so the placeholder pane
            // lands in the worktree (mirrors the iTerm/Terminal.app open scripts).
            let _ = run_argv("tmux", &tmux_cd_argv(&session, &cwd));
            Ok(ItemSurface::Tmux { session })
        }
        TerminalContext::Zellij => {
            let _g = focus_guard();
            run_argv("zellij", &zellij_open_argv(key, &cwd)).context("zellij new-tab")?;
            Ok(ItemSurface::Zellij { tab: key.to_string() })
        }
        TerminalContext::ITerm2 => {
            let id = osascript_capture(&iterm_open_script(key, &cwd)).context("iTerm2 new window")?;
            Ok(ItemSurface::ITerm2 { window_id: id })
        }
        TerminalContext::AppleTerminal => {
            let _g = focus_guard();
            let id = osascript_capture(&apple_open_script(key, &cwd)).unwrap_or_default();
            Ok(ItemSurface::Apple { window_id: id })
        }
    }
}

/// Launch `bash <script_path>` as a new pane/tab named `role` inside `surface`.
/// `close_on_exit=false` keeps the pane after a clean exit (dry mode).
pub fn launch_agent_in_surface(
    surface: &ItemSurface,
    role: &str,
    script_path: &Path,
    close_on_exit: bool,
) -> Result<()> {
    let script = script_path.display().to_string();
    let run_cmd = format!("bash '{script}'");
    match surface {
        ItemSurface::Tmux { session } => {
            run_argv("tmux", &tmux_launch_argv(session, &run_cmd)).context("tmux split-window")?;
            // Best-effort pane title + readable layout (ignore failures).
            let _ = run_argv("tmux", &["select-pane", "-t", session, "-T", role].map(String::from));
            let _ = run_argv("tmux", &["select-layout", "-t", session, "tiled"].map(String::from));
            Ok(())
        }
        ItemSurface::Zellij { tab } => {
            let _g = focus_guard();
            run_argv("zellij", &zellij_goto_argv(tab)).context("zellij go-to-tab-name")?;
            run_argv("zellij", &zellij_run_argv(role, &script, close_on_exit))
                .context("zellij run")?;
            Ok(())
        }
        ItemSurface::ITerm2 { window_id } => {
            if window_id.is_empty() {
                return launch_agent_window(TerminalContext::ITerm2, role, script_path);
            }
            run_argv("osascript", &["-e".to_string(), iterm_launch_script(window_id, role, &run_cmd)])
                .context("iTerm2 new tab")
        }
        ItemSurface::Apple { .. } => {
            // Terminal.app has no clean create-tab-in-window verb — best-effort
            // new window per agent, titled by role.
            let _g = focus_guard();
            run_argv("osascript", &["-e".to_string(), apple_launch_script(role, &run_cmd)])
                .context("Terminal.app window")
        }
    }
}

/// Tear down an item container and everything running in it (the agents). Called
/// on `q` abort. Best-effort per surface — the caller logs and continues.
pub fn kill_item_surface(surface: &ItemSurface) -> Result<()> {
    match surface {
        ItemSurface::Tmux { session } => {
            run_argv("tmux", &["kill-session", "-t", session].map(String::from))
                .context("tmux kill-session")
        }
        ItemSurface::Zellij { tab } => {
            let _g = focus_guard();
            run_argv("zellij", &zellij_goto_argv(tab)).context("zellij go-to-tab-name")?;
            run_argv("zellij", &["action", "close-tab"].map(String::from))
                .context("zellij close-tab")
        }
        ItemSurface::ITerm2 { window_id } => {
            if window_id.is_empty() {
                return Ok(());
            }
            run_argv(
                "osascript",
                &["-e".to_string(), format!("tell application \"iTerm\" to close (window id {window_id})")],
            )
            .context("iTerm2 close window")
        }
        ItemSurface::Apple { window_id } => {
            if window_id.is_empty() {
                return Ok(());
            }
            let _g = focus_guard();
            run_argv(
                "osascript",
                &["-e".to_string(), format!("tell application \"Terminal\" to close window id {window_id}")],
            )
            .context("Terminal.app close window")
        }
    }
}

fn run_argv(program: &str, args: &[String]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("spawn {program}"))?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    Ok(())
}

/// Run an AppleScript and return its trimmed stdout (e.g. a window id).
fn osascript_capture(script: &str) -> Result<String> {
    let out = Command::new("osascript")
        .args(["-e", script])
        .output()
        .context("spawn osascript")?;
    if !out.status.success() {
        bail!(
            "osascript failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn tmux_open_argv(session: &str, cwd: &str) -> Vec<String> {
    ["new-session", "-d", "-s", session, "-c", cwd].map(String::from).to_vec()
}

fn tmux_launch_argv(session: &str, run_cmd: &str) -> Vec<String> {
    ["split-window", "-t", session, run_cmd].map(String::from).to_vec()
}

/// Send an explicit `cd` into the session's (placeholder) pane after startup, so a
/// profile that `cd`s during shell init cannot leave it outside the worktree.
fn tmux_cd_argv(session: &str, cwd: &str) -> Vec<String> {
    ["send-keys", "-t", session, &format!("cd '{cwd}'; clear"), "Enter"]
        .map(String::from)
        .to_vec()
}

fn zellij_open_argv(tab: &str, cwd: &str) -> Vec<String> {
    ["action", "new-tab", "--name", tab, "--cwd", cwd].map(String::from).to_vec()
}

fn zellij_goto_argv(tab: &str) -> Vec<String> {
    ["action", "go-to-tab-name", tab].map(String::from).to_vec()
}

fn zellij_run_argv(role: &str, script: &str, close_on_exit: bool) -> Vec<String> {
    let mut v = vec!["run".to_string()];
    if close_on_exit {
        v.push("--close-on-exit".to_string());
    }
    v.extend(["--name", role, "--", "bash", script].map(String::from));
    v
}

fn iterm_open_script(key: &str, cwd: &str) -> String {
    format!(
        "tell application \"iTerm\"\n\
         \tset w to (create window with default profile)\n\
         \ttell current session of w to write text \"cd '{cwd}'; clear; echo 'scrutiny forge bulk: {key}'\"\n\
         \treturn id of w\n\
         end tell"
    )
}

fn iterm_launch_script(window_id: &str, role: &str, run_cmd: &str) -> String {
    format!(
        "tell application \"iTerm\"\n\
         \ttell window id {window_id}\n\
         \t\tcreate tab with default profile\n\
         \t\tset name of current session to \"{role}\"\n\
         \t\ttell current session to write text \"{run_cmd}\"\n\
         \tend tell\n\
         end tell"
    )
}

fn apple_open_script(key: &str, cwd: &str) -> String {
    format!(
        "tell application \"Terminal\"\n\
         \tset w to do script \"cd '{cwd}'; clear; echo 'scrutiny forge bulk: {key}'\"\n\
         \tset custom title of w to \"{key}\"\n\
         \treturn id of (window 1 whose tabs contains w)\n\
         end tell"
    )
}

fn apple_launch_script(role: &str, run_cmd: &str) -> String {
    format!(
        "tell application \"Terminal\"\n\
         \tset t to do script \"{run_cmd}\"\n\
         \tset custom title of t to \"{role}\"\n\
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

    #[test]
    fn tmux_argv_targets_session_by_name() {
        assert_eq!(
            tmux_open_argv("nero-8729", "/tmp/wt"),
            vec!["new-session", "-d", "-s", "nero-8729", "-c", "/tmp/wt"]
        );
        assert_eq!(
            tmux_launch_argv("nero-8729", "bash '/tmp/s.sh'"),
            vec!["split-window", "-t", "nero-8729", "bash '/tmp/s.sh'"]
        );
    }

    #[test]
    fn tmux_cd_argv_sends_cd_and_enter() {
        assert_eq!(
            tmux_cd_argv("nero-8729", "/tmp/wt"),
            vec!["send-keys", "-t", "nero-8729", "cd '/tmp/wt'; clear", "Enter"]
        );
    }

    #[test]
    fn zellij_run_argv_toggles_close_on_exit() {
        assert_eq!(
            zellij_run_argv("developer", "/tmp/s.sh", true),
            vec!["run", "--close-on-exit", "--name", "developer", "--", "bash", "/tmp/s.sh"]
        );
        assert_eq!(
            zellij_run_argv("developer", "/tmp/s.sh", false),
            vec!["run", "--name", "developer", "--", "bash", "/tmp/s.sh"]
        );
    }

    #[test]
    fn zellij_open_names_tab_and_cwd() {
        assert_eq!(
            zellij_open_argv("PROJ-1", "/tmp/wt"),
            vec!["action", "new-tab", "--name", "PROJ-1", "--cwd", "/tmp/wt"]
        );
    }
}
