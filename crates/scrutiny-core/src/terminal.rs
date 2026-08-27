//! Detect the terminal surface and launch a visible agent window on it.
//!
//! Used by non-headless parley/probe/forge (`headless = false`): each agent
//! runs in its own visible window/pane (claude/cursor) instead of a captured
//! headless child.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalContext {
    Tmux,
    Zellij,
    ITerm2,
    AppleTerminal,
}

/// Origin tab/window captured once at [`resolve_terminal`] so later spawns do not
/// follow the user's currently focused tab.
#[derive(Debug, Clone)]
pub struct ZellijAnchor {
    /// Tab name (always set when capture succeeds).
    pub tab_name: String,
    /// Stable tab id when the installed zellij reports it.
    pub tab_id: Option<u32>,
    /// Tab that was focused at capture time (restore after legacy goto+run).
    pub restore_tab_name: Option<String>,
}

/// Origin tmux session:window for non-bulk splits.
#[derive(Debug, Clone)]
pub struct TmuxAnchor {
    /// `session_name:#{window_id}` (e.g. `main:@2`).
    pub target: String,
}

/// Resolved non-headless surface, with origin anchors for multiplexers.
#[derive(Debug, Clone)]
pub struct ResolvedTerminal {
    pub kind: TerminalContext,
    pub zellij: Option<ZellijAnchor>,
    pub tmux: Option<TmuxAnchor>,
}

/// A per-item terminal container (one per bulk ticket): agents for that item are
/// launched into it so panes stay grouped. See [`open_item_surface`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ItemSurface {
    /// Detached tmux session named after the item key.
    Tmux { session: String },
    /// Zellij tab named after the item key (+ optional stable tab id).
    Zellij {
        tab: String,
        #[serde(default)]
        tab_id: Option<u32>,
    },
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

/// Detected zellij CLI capabilities (probed once).
#[derive(Debug, Clone, Copy, Default)]
struct ZellijCaps {
    near_current_pane: bool,
    tab_id: bool,
    no_focus: bool,
    current_tab_info: bool,
}

fn zellij_caps() -> ZellijCaps {
    static CAPS: OnceLock<ZellijCaps> = OnceLock::new();
    *CAPS.get_or_init(probe_zellij_caps)
}

fn probe_zellij_caps() -> ZellijCaps {
    let run_help = Command::new("zellij")
        .args(["run", "--help"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
        })
        .unwrap_or_default();
    let new_pane_help = Command::new("zellij")
        .args(["action", "new-pane", "--help"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
        })
        .unwrap_or_default();
    let action_help = Command::new("zellij")
        .args(["action", "--help"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
        })
        .unwrap_or_default();
    ZellijCaps {
        near_current_pane: run_help.contains("near-current-pane")
            || new_pane_help.contains("near-current-pane"),
        tab_id: new_pane_help.contains("--tab-id") || new_pane_help.contains("tab-id"),
        no_focus: new_pane_help.contains("no-focus") || run_help.contains("no-focus"),
        current_tab_info: action_help.contains("current-tab-info"),
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

/// Clients that can run in a visible tmux/zellij/iTerm2/Terminal.app pane.
/// Codex stays headless (`codex exec`).
pub fn supports_visible_terminal(client: &str) -> bool {
    matches!(client, "claude" | "cursor")
}

/// Decide the non-headless terminal surface for a spawned agent, or `None` to
/// run headless. Captures origin tab/window anchors for multiplexers.
///
/// `None` when: `headless = true`; the client is not claude/cursor (Codex stays
/// headless); or no supported surface is detected.
pub fn resolve_terminal(headless: bool, client: &str, tool: &str) -> Option<ResolvedTerminal> {
    if headless {
        return None;
    }
    match detect_terminal() {
        Some(_) if !supports_visible_terminal(client) => {
            eprintln!(
                "scrutiny {tool}: headless=false but non-headless mode supports claude/cursor only \
                 (got {client}) — running headless"
            );
            None
        }
        Some(kind) => {
            let mut resolved = ResolvedTerminal {
                kind,
                zellij: None,
                tmux: None,
            };
            match kind {
                TerminalContext::Zellij => {
                    resolved.zellij = capture_zellij_anchor();
                    let caps = zellij_caps();
                    if !caps.near_current_pane && !caps.tab_id {
                        eprintln!(
                            "scrutiny {tool}: zellij lacks --near-current-pane/--tab-id \
                             (upgrade to ≥0.44 for spawn without focus steal); \
                             using origin-tab goto fallback"
                        );
                    }
                }
                TerminalContext::Tmux => {
                    resolved.tmux = capture_tmux_anchor();
                }
                _ => {}
            }
            eprintln!(
                "scrutiny {tool}: headless=false — opening agents in {kind:?} windows (auto mode)"
            );
            Some(resolved)
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

fn zellij_session_args() -> Vec<String> {
    match std::env::var("ZELLIJ_SESSION_NAME") {
        Ok(s) if !s.is_empty() => vec!["--session".into(), s],
        _ => Vec::new(),
    }
}

fn zellij_cmd(args: &[String]) -> Command {
    let mut c = Command::new("zellij");
    for a in zellij_session_args() {
        c.arg(a);
    }
    for a in args {
        c.arg(a);
    }
    c
}

fn capture_zellij_anchor() -> Option<ZellijAnchor> {
    let caps = zellij_caps();
    let restore = focused_tab_name_from_layout();

    if caps.current_tab_info {
        if let Some(a) = capture_via_current_tab_info(restore.clone()) {
            return Some(a);
        }
    }

    let cwd = std::env::current_dir().ok()?;
    let layout = zellij_dump_layout()?;
    let tab_name = origin_tab_from_layout(&layout, &cwd)?;
    Some(ZellijAnchor {
        tab_name,
        tab_id: None,
        restore_tab_name: restore,
    })
}

fn capture_via_current_tab_info(restore: Option<String>) -> Option<ZellijAnchor> {
    let mut args = zellij_session_args();
    args.extend(["action".into(), "current-tab-info".into()]);
    let out = Command::new("zellij").args(&args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut name = None;
    let mut id = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name:") {
            name = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("id:") {
            id = rest.trim().parse().ok();
        }
    }
    Some(ZellijAnchor {
        tab_name: name?,
        tab_id: id,
        restore_tab_name: restore,
    })
}

fn zellij_dump_layout() -> Option<String> {
    let mut args = zellij_session_args();
    args.extend(["action".into(), "dump-layout".into()]);
    let out = Command::new("zellij").args(&args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn focused_tab_name_from_layout() -> Option<String> {
    let layout = zellij_dump_layout()?;
    for line in layout.lines() {
        let t = line.trim();
        if t.starts_with("tab ") && t.contains("focus=true") {
            if let Some(name) = tab_name_from_header(t) {
                return Some(name);
            }
        }
    }
    None
}

/// Parse dump-layout for the tab that hosts a pane whose cwd matches `cwd`.
/// Prefers a non-focused match when the focused tab is elsewhere (user browsing).
pub fn origin_tab_from_layout(layout: &str, cwd: &Path) -> Option<String> {
    let cwd_s = cwd.to_string_lossy();
    let cwd_end = cwd.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let mut matches: Vec<(String, bool)> = Vec::new(); // (name, tab_focused)

    let mut current_tab: Option<(String, bool)> = None;
    for line in layout.lines() {
        let t = line.trim();
        if t.starts_with("tab ") {
            let focused = t.contains("focus=true");
            if let Some(name) = tab_name_from_header(t) {
                current_tab = Some((name, focused));
            } else {
                current_tab = None;
            }
            continue;
        }
        let Some((tab_name, tab_focused)) = &current_tab else {
            continue;
        };
        if !t.contains("cwd=") {
            continue;
        }
        let pane_cwd = cwd_attr(t).unwrap_or_default();
        if pane_cwd.is_empty() {
            continue;
        }
        let hit = pane_cwd == cwd_s
            || pane_cwd.ends_with(cwd_s.as_ref())
            || (!cwd_end.is_empty() && pane_cwd.ends_with(cwd_end))
            || PathBuf::from(&pane_cwd) == cwd;
        if hit {
            matches.push((tab_name.clone(), *tab_focused));
        }
    }

    if matches.is_empty() {
        return None;
    }
    // Prefer a match whose tab is not the (possibly wrong) focused tab when
    // multiple hits exist; otherwise take the first.
    if matches.len() > 1 {
        if let Some((name, _)) = matches.iter().find(|(_, focused)| !*focused) {
            return Some(name.clone());
        }
    }
    Some(matches[0].0.clone())
}

fn tab_name_from_header(line: &str) -> Option<String> {
    let key = "name=\"";
    let start = line.find(key)? + key.len();
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_string())
}

fn cwd_attr(line: &str) -> Option<String> {
    let key = "cwd=\"";
    let start = line.find(key)? + key.len();
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_string())
}

fn capture_tmux_anchor() -> Option<TmuxAnchor> {
    let pane = std::env::var("TMUX_PANE").ok()?;
    if pane.is_empty() {
        return None;
    }
    let out = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{session_name}:#{window_id}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let target = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if target.is_empty() || !target.contains(':') {
        return None;
    }
    Some(TmuxAnchor { target })
}

/// Open a new visible window/session running `bash <script_path>` on `ctx`.
///
/// Returns once the launcher command exits — the agent keeps running in its own
/// window; the host waits on the agent's completion sentinel, not on this call.
pub fn launch_agent_window(ctx: &ResolvedTerminal, label: &str, script_path: &Path) -> Result<()> {
    let script = script_path.display().to_string();
    let run_cmd = format!("bash '{script}'");
    match ctx.kind {
        TerminalContext::Tmux => {
            let target = ctx.tmux.as_ref().map(|a| a.target.as_str()).unwrap_or("");
            let status = if target.is_empty() {
                // Fallback: dedicated detached session (legacy).
                Command::new("tmux")
                    .args([
                        "new-session",
                        "-d",
                        "-s",
                        &tmux_session_name(label),
                        &run_cmd,
                    ])
                    .status()
                    .context("spawn tmux new-session")?
            } else {
                let st = Command::new("tmux")
                    .args(["split-window", "-t", target, &run_cmd])
                    .status()
                    .context("spawn tmux split-window")?;
                let _ = Command::new("tmux")
                    .args(["select-pane", "-T", label, "-t", target])
                    .status();
                st
            };
            if !status.success() {
                bail!("terminal launcher for {label} exited with {status}");
            }
            Ok(())
        }
        TerminalContext::Zellij => {
            launch_zellij_in_origin(ctx.zellij.as_ref(), label, &script, true)
        }
        TerminalContext::AppleTerminal => {
            let status = Command::new("osascript")
                .args([
                    "-e",
                    &format!("tell application \"Terminal\" to do script \"{run_cmd}\""),
                ])
                .status()
                .context("spawn Terminal.app window")?;
            if !status.success() {
                bail!("terminal launcher for {label} exited with {status}");
            }
            Ok(())
        }
        TerminalContext::ITerm2 => {
            let status = Command::new("osascript")
                .args(["-e", &iterm2_new_window_script(&run_cmd)])
                .status()
                .context("spawn iTerm2 window")?;
            if !status.success() {
                bail!("terminal launcher for {label} exited with {status}");
            }
            Ok(())
        }
    }
}

fn launch_zellij_in_origin(
    anchor: Option<&ZellijAnchor>,
    label: &str,
    script: &str,
    close_on_exit: bool,
) -> Result<()> {
    let caps = zellij_caps();

    // Best path: open beside the invoking pane without following user focus.
    if caps.near_current_pane {
        let args = zellij_run_near_argv(label, script, close_on_exit);
        let status = zellij_cmd(&args)
            .status()
            .context("spawn zellij run --near-current-pane")?;
        if !status.success() {
            bail!("zellij run for {label} exited with {status}");
        }
        return Ok(());
    }

    // Next: target a known tab id without stealing focus.
    if let Some(a) = anchor {
        if caps.tab_id {
            if let Some(id) = a.tab_id {
                let args =
                    zellij_new_pane_tab_id_argv(id, label, script, close_on_exit, caps.no_focus);
                let status = zellij_cmd(&args)
                    .status()
                    .context("spawn zellij action new-pane --tab-id")?;
                if !status.success() {
                    bail!("zellij new-pane for {label} exited with {status}");
                }
                return Ok(());
            }
        }
    }

    // Legacy: go-to origin tab name, run, restore prior focus.
    let Some(a) = anchor else {
        let args = zellij_run_argv(label, script, close_on_exit);
        let status = zellij_cmd(&args).status().context("spawn zellij run")?;
        if !status.success() {
            bail!("zellij run for {label} exited with {status}");
        }
        return Ok(());
    };

    let _g = focus_guard();
    run_zellij_argv(&zellij_goto_argv(&a.tab_name)).context("zellij go-to-tab-name origin")?;
    run_zellij_argv(&zellij_run_argv(label, script, close_on_exit)).context("zellij run")?;
    if let Some(prev) = &a.restore_tab_name {
        if prev != &a.tab_name {
            let _ = run_zellij_argv(&zellij_goto_argv(prev));
        }
    }
    Ok(())
}

fn run_zellij_argv(args: &[String]) -> Result<()> {
    let status = zellij_cmd(args).status().context("spawn zellij")?;
    if !status.success() {
        bail!("zellij exited with {status}");
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
pub fn open_item_surface(ctx: &ResolvedTerminal, key: &str, cwd: &Path) -> Result<ItemSurface> {
    let cwd = cwd.display().to_string();
    match ctx.kind {
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
            let caps = zellij_caps();
            let _g = focus_guard();
            let tab_id = if caps.tab_id || caps.current_tab_info {
                // Prefer capturing id from new-tab stdout when available.
                capture_new_tab_id(key, &cwd)
            } else {
                None
            };
            if tab_id.is_none() {
                run_zellij_argv(&zellij_open_argv(key, &cwd)).context("zellij new-tab")?;
            }
            Ok(ItemSurface::Zellij {
                tab: key.to_string(),
                tab_id,
            })
        }
        TerminalContext::ITerm2 => {
            let id =
                osascript_capture(&iterm_open_script(key, &cwd)).context("iTerm2 new window")?;
            Ok(ItemSurface::ITerm2 { window_id: id })
        }
        TerminalContext::AppleTerminal => {
            let _g = focus_guard();
            let id = osascript_capture(&apple_open_script(key, &cwd)).unwrap_or_default();
            Ok(ItemSurface::Apple { window_id: id })
        }
    }
}

fn capture_new_tab_id(tab: &str, cwd: &str) -> Option<u32> {
    let mut args = zellij_session_args();
    args.extend([
        "action".into(),
        "new-tab".into(),
        "--name".into(),
        tab.into(),
        "--cwd".into(),
        cwd.into(),
    ]);
    let out = Command::new("zellij").args(&args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Newer zellij prints the tab id alone or as `id: N`.
    for tok in text.split_whitespace() {
        if let Ok(id) = tok.parse::<u32>() {
            return Some(id);
        }
        if let Some(rest) = tok.strip_prefix("id:") {
            if let Ok(id) = rest.parse::<u32>() {
                return Some(id);
            }
        }
    }
    let trimmed = text.trim();
    trimmed.parse().ok()
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
            let _ = run_argv(
                "tmux",
                &["select-pane", "-t", session, "-T", role].map(String::from),
            );
            let _ = run_argv(
                "tmux",
                &["select-layout", "-t", session, "tiled"].map(String::from),
            );
            Ok(())
        }
        ItemSurface::Zellij { tab, tab_id } => {
            let caps = zellij_caps();
            if caps.tab_id {
                if let Some(id) = tab_id {
                    let args = zellij_new_pane_tab_id_argv(
                        *id,
                        role,
                        &script,
                        close_on_exit,
                        caps.no_focus,
                    );
                    return run_zellij_argv(&args).context("zellij new-pane --tab-id");
                }
            }
            let _g = focus_guard();
            run_zellij_argv(&zellij_goto_argv(tab)).context("zellij go-to-tab-name")?;
            run_zellij_argv(&zellij_run_argv(role, &script, close_on_exit))
                .context("zellij run")?;
            Ok(())
        }
        ItemSurface::ITerm2 { window_id } => {
            if window_id.is_empty() {
                let fallback = ResolvedTerminal {
                    kind: TerminalContext::ITerm2,
                    zellij: None,
                    tmux: None,
                };
                return launch_agent_window(&fallback, role, script_path);
            }
            run_argv(
                "osascript",
                &[
                    "-e".to_string(),
                    iterm_launch_script(window_id, role, &run_cmd),
                ],
            )
            .context("iTerm2 new tab")
        }
        ItemSurface::Apple { .. } => {
            // Terminal.app has no clean create-tab-in-window verb — best-effort
            // new window per agent, titled by role.
            let _g = focus_guard();
            run_argv(
                "osascript",
                &["-e".to_string(), apple_launch_script(role, &run_cmd)],
            )
            .context("Terminal.app window")
        }
    }
}

/// Shell command that closes the current pane/window from inside the agent script.
/// Embedded in the success branch so the terminal closes when the agent finishes.
///
/// Tmux: `kill-pane` targets `$TMUX_PANE` (inherited by all processes in the pane).
/// Zellij: `:` no-op — the pane is launched with `--close-on-exit` so it closes
///         automatically when bash exits; no explicit close command needed.
/// iTerm2/Terminal.app: osascript closes the window the script is running in.
pub fn kill_cmd_for_terminal(ctx: &ResolvedTerminal) -> String {
    match ctx.kind {
        TerminalContext::Tmux => "tmux kill-pane".into(),
        TerminalContext::Zellij => ":".into(),
        TerminalContext::ITerm2 => {
            "osascript -e 'tell application \"iTerm\" to close (current window)'".into()
        }
        TerminalContext::AppleTerminal => {
            "osascript -e 'tell application \"Terminal\" to close front window'".into()
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
        ItemSurface::Zellij { tab, tab_id } => {
            let caps = zellij_caps();
            if caps.tab_id {
                if let Some(id) = tab_id {
                    let mut args = zellij_session_args();
                    args.extend([
                        "action".into(),
                        "close-tab".into(),
                        "--tab-id".into(),
                        id.to_string(),
                    ]);
                    return run_zellij_argv(&args).context("zellij close-tab --tab-id");
                }
            }
            let _g = focus_guard();
            run_zellij_argv(&zellij_goto_argv(tab)).context("zellij go-to-tab-name")?;
            run_zellij_argv(&["action".into(), "close-tab".into()]).context("zellij close-tab")
        }
        ItemSurface::ITerm2 { window_id } => {
            if window_id.is_empty() {
                return Ok(());
            }
            run_argv(
                "osascript",
                &[
                    "-e".to_string(),
                    format!("tell application \"iTerm\" to close (window id {window_id})"),
                ],
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
                &[
                    "-e".to_string(),
                    format!("tell application \"Terminal\" to close window id {window_id}"),
                ],
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
    ["new-session", "-d", "-s", session, "-c", cwd]
        .map(String::from)
        .to_vec()
}

fn tmux_launch_argv(session: &str, run_cmd: &str) -> Vec<String> {
    ["split-window", "-t", session, run_cmd]
        .map(String::from)
        .to_vec()
}

/// Non-bulk: split into the origin session:window.
pub fn tmux_split_origin_argv(target: &str, run_cmd: &str) -> Vec<String> {
    ["split-window", "-t", target, run_cmd]
        .map(String::from)
        .to_vec()
}

/// Send an explicit `cd` into the session's (placeholder) pane after startup, so a
/// profile that `cd`s during shell init cannot leave it outside the worktree.
fn tmux_cd_argv(session: &str, cwd: &str) -> Vec<String> {
    [
        "send-keys",
        "-t",
        session,
        &format!("cd '{cwd}'; clear"),
        "Enter",
    ]
    .map(String::from)
    .to_vec()
}

fn zellij_open_argv(tab: &str, cwd: &str) -> Vec<String> {
    ["action", "new-tab", "--name", tab, "--cwd", cwd]
        .map(String::from)
        .to_vec()
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

pub fn zellij_run_near_argv(role: &str, script: &str, close_on_exit: bool) -> Vec<String> {
    let mut v = vec!["run".to_string(), "--near-current-pane".to_string()];
    if close_on_exit {
        v.push("--close-on-exit".to_string());
    }
    v.extend(["--name", role, "--", "bash", script].map(String::from));
    v
}

pub fn zellij_new_pane_tab_id_argv(
    tab_id: u32,
    role: &str,
    script: &str,
    close_on_exit: bool,
    no_focus: bool,
) -> Vec<String> {
    let mut v = vec![
        "action".into(),
        "new-pane".into(),
        "--tab-id".into(),
        tab_id.to_string(),
        "--name".into(),
        role.into(),
    ];
    if no_focus {
        v.push("--no-focus".into());
    }
    if close_on_exit {
        v.push("--close-on-exit".into());
    }
    v.extend(["--".into(), "bash".into(), script.into()]);
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
        assert!(resolve_terminal(true, "claude", "probe").is_none());
        assert!(resolve_terminal(true, "cursor", "probe").is_none());
    }

    #[test]
    fn supports_visible_terminal_claude_and_cursor() {
        assert!(supports_visible_terminal("claude"));
        assert!(supports_visible_terminal("cursor"));
        assert!(!supports_visible_terminal("codex"));
        assert!(!supports_visible_terminal("unknown"));
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
        assert_eq!(
            tmux_split_origin_argv("main:@2", "bash '/tmp/s.sh'"),
            vec!["split-window", "-t", "main:@2", "bash '/tmp/s.sh'"]
        );
    }

    #[test]
    fn tmux_cd_argv_sends_cd_and_enter() {
        assert_eq!(
            tmux_cd_argv("nero-8729", "/tmp/wt"),
            vec![
                "send-keys",
                "-t",
                "nero-8729",
                "cd '/tmp/wt'; clear",
                "Enter"
            ]
        );
    }

    #[test]
    fn zellij_run_argv_toggles_close_on_exit() {
        assert_eq!(
            zellij_run_argv("developer", "/tmp/s.sh", true),
            vec![
                "run",
                "--close-on-exit",
                "--name",
                "developer",
                "--",
                "bash",
                "/tmp/s.sh"
            ]
        );
        assert_eq!(
            zellij_run_argv("developer", "/tmp/s.sh", false),
            vec!["run", "--name", "developer", "--", "bash", "/tmp/s.sh"]
        );
    }

    #[test]
    fn zellij_run_near_includes_flag() {
        assert_eq!(
            zellij_run_near_argv("reviewer", "/tmp/s.sh", true),
            vec![
                "run",
                "--near-current-pane",
                "--close-on-exit",
                "--name",
                "reviewer",
                "--",
                "bash",
                "/tmp/s.sh"
            ]
        );
    }

    #[test]
    fn zellij_new_pane_tab_id_argv_shape() {
        assert_eq!(
            zellij_new_pane_tab_id_argv(3, "dev", "/tmp/x.sh", true, true),
            vec![
                "action",
                "new-pane",
                "--tab-id",
                "3",
                "--name",
                "dev",
                "--no-focus",
                "--close-on-exit",
                "--",
                "bash",
                "/tmp/x.sh"
            ]
        );
    }

    #[test]
    fn zellij_open_names_tab_and_cwd() {
        assert_eq!(
            zellij_open_argv("PROJ-1", "/tmp/wt"),
            vec!["action", "new-tab", "--name", "PROJ-1", "--cwd", "/tmp/wt"]
        );
    }

    #[test]
    fn origin_tab_from_layout_prefers_cwd_match_over_focus() {
        let layout = r#"
layout {
    tab name="Scrutiny" hide_floating_panes=true {
        pane command="agent" cwd="/Users/me/dev/scrutiny" {
        }
    }
    tab name="NERO-617" focus=true hide_floating_panes=true {
        pane cwd="/Users/me/other" focus=true size="50%"
    }
}
"#;
        let origin = origin_tab_from_layout(layout, Path::new("/Users/me/dev/scrutiny"));
        assert_eq!(origin.as_deref(), Some("Scrutiny"));
    }
}
