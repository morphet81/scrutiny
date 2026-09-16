//! Detect the terminal surface and launch a visible agent window on it.
//!
//! Used by non-headless parley/probe/forge (`headless = false`): each agent
//! runs in its own visible window/pane (claude/cursor) instead of a captured
//! headless child.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::thread;
use std::time::Duration;

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

/// Visible agent pane tracked so the host can force-close leftovers on exit.
#[derive(Debug, Clone)]
struct TrackedAgentPane {
    label: String,
    pid_path: PathBuf,
    /// Substring that must appear in the process cmdline (usually the agent script path).
    script_marker: String,
}

static TRACKED_AGENT_PANES: Mutex<Vec<TrackedAgentPane>> = Mutex::new(Vec::new());
static CLEANUP_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static CLEANUP_ONCE: Once = Once::new();
/// Set by the SIGINT/SIGTERM handler. Real pane teardown runs on Drop / explicit
/// [`force_close_agent_panes`] — never inside the signal handler (async-unsafe).
static CLEANUP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Register a non-headless agent pane (bash PID written to `pid_path` by the script).
/// `script_marker` is matched against `ps` cmdline before any kill so a recycled
/// PID cannot take down zellij or unrelated processes.
pub fn register_agent_pane(label: &str, pid_path: PathBuf, script_marker: impl Into<String>) {
    install_agent_pane_cleanup_hook();
    if let Ok(mut g) = TRACKED_AGENT_PANES.lock() {
        g.push(TrackedAgentPane {
            label: label.to_string(),
            pid_path,
            script_marker: script_marker.into(),
        });
    }
}

/// True after Ctrl-C / SIGTERM was observed (flag-only handler).
pub fn cleanup_requested() -> bool {
    CLEANUP_REQUESTED.load(Ordering::SeqCst)
}

/// Kill still-running agent pane processes so `--close-on-exit` panes disappear.
/// Safe to call multiple times; no-ops when nothing is tracked or PIDs already dead.
pub fn force_close_agent_panes() {
    let panes = match TRACKED_AGENT_PANES.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(_) => return,
    };
    if panes.is_empty() {
        return;
    }
    let mut closed = 0u32;
    for pane in &panes {
        if kill_agent_pane_pidfile(&pane.pid_path, &pane.script_marker) {
            closed += 1;
            eprintln!("scrutiny: force-closed agent pane {}", pane.label);
        }
        let _ = fs::remove_file(&pane.pid_path);
    }
    if closed == 0 && !panes.is_empty() {
        // PIDs already gone (auto --close-on-exit) — silent.
    } else if closed > 1 {
        eprintln!("scrutiny: force-closed {closed} agent pane(s) total");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcIdentity {
    start: String,
    cmd: String,
}

/// Snapshot start-time + cmdline for `pid`. `None` if the process is gone.
fn read_proc_identity(pid: i32) -> Option<ProcIdentity> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart=", "-o", "command="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    // lstart is typically "Day Mon DD HH:MM:SS YYYY" then cmdline.
    // Split on the year token (4 digits) that ends lstart.
    let mut year_end = None;
    for (i, w) in line.split_whitespace().enumerate() {
        if i >= 4 && w.len() == 4 && w.chars().all(|c| c.is_ascii_digit()) {
            year_end = Some(i);
            break;
        }
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    let (start, cmd) = if let Some(yi) = year_end {
        if yi + 1 >= parts.len() {
            return None;
        }
        (parts[..=yi].join(" "), parts[yi + 1..].join(" "))
    } else {
        // Fallback: treat whole line as cmdline (still useful for marker check).
        (String::new(), line.to_string())
    };
    if cmd.is_empty() {
        return None;
    }
    Some(ProcIdentity { start, cmd })
}

fn identity_matches_marker(id: &ProcIdentity, marker: &str) -> bool {
    if marker.is_empty() {
        // No marker → only allow obvious agent wrapper shells.
        return id.cmd.contains("agent-script")
            || id.cmd.contains("forge-all-driver")
            || id.cmd.contains("bulk-driver");
    }
    id.cmd.contains(marker)
}

fn pid_alive(pid: i32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        read_proc_identity(pid).is_some()
    }
}

fn kill_agent_pane_pidfile(pid_path: &Path, script_marker: &str) -> bool {
    let Ok(raw) = fs::read_to_string(pid_path) else {
        return false;
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return false;
    };
    if pid <= 1 {
        return false;
    }
    // Already dead (normal --close-on-exit) — do not SIGKILL a recycled PID.
    if !pid_alive(pid) {
        return false;
    }
    let Some(id_before) = read_proc_identity(pid) else {
        return false;
    };
    if !identity_matches_marker(&id_before, script_marker) {
        eprintln!(
            "scrutiny: skip pane kill pid={pid}: cmdline does not match agent marker"
        );
        return false;
    }

    #[cfg(unix)]
    {
        // Never process-group kill: `kill(-pid)` targets PGID==pid and has
        // nuked the zellij client when a recycled pid collided.
        let signaled = unsafe { libc::kill(pid, libc::SIGTERM) == 0 };
        if !signaled {
            return false;
        }
        // Poll for exit; re-check identity before any SIGKILL (PID reuse TOCTOU).
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(25));
            if !pid_alive(pid) {
                return true;
            }
        }
        let Some(id_after) = read_proc_identity(pid) else {
            return true; // exited between checks
        };
        if id_after != id_before || !identity_matches_marker(&id_after, script_marker) {
            eprintln!(
                "scrutiny: skip SIGKILL pid={pid}: process identity changed (possible PID reuse)"
            );
            return false;
        }
        unsafe {
            let _ = libc::kill(pid, libc::SIGKILL);
        }
        true
    }
    #[cfg(windows)]
    {
        let _ = id_before;
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, id_before);
        false
    }
}

/// RAII: force-close any tracked agent panes when dropped (normal exit / unwind).
pub struct AgentPaneCleanupGuard;

impl Default for AgentPaneCleanupGuard {
    fn default() -> Self {
        install_agent_pane_cleanup_hook();
        Self
    }
}

impl Drop for AgentPaneCleanupGuard {
    fn drop(&mut self) {
        force_close_agent_panes();
    }
}

fn install_agent_pane_cleanup_hook() {
    if CLEANUP_HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    CLEANUP_ONCE.call_once(|| {
        #[cfg(unix)]
        unsafe {
            // Flag only — no mutex / sleep / kill in the handler (async-signal-safe).
            // Pane teardown happens via AgentPaneCleanupGuard Drop on unwind paths
            // that still run, or explicit force_close after agent waits.
            let handler: libc::sighandler_t =
                agent_pane_signal_handler as *const () as libc::sighandler_t;
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }
    });
}

#[cfg(unix)]
extern "C" fn agent_pane_signal_handler(sig: libc::c_int) {
    CLEANUP_REQUESTED.store(true, Ordering::SeqCst);
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        let _ = libc::raise(sig);
    }
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
    list_panes: bool,
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
        list_panes: action_help.contains("list-panes"),
    }
}

/// True when zellij must steal focus for every pane spawn (&lt;0.44).
/// Callers should serialize multi-item launches to avoid session thrash/crashes.
pub fn zellij_needs_serial_launches() -> bool {
    if detect_terminal() != Some(TerminalContext::Zellij) {
        return false;
    }
    let caps = zellij_caps();
    !caps.near_current_pane && !caps.tab_id
}

/// Env var pointing at a JSON [`ItemSurface`] for nested `scrutiny forge --here`.
pub const ITEM_SURFACE_ENV: &str = "SCRUTINY_ITEM_SURFACE";

/// Load a per-item surface written by multi-ticket forge drivers, if present.
pub fn load_item_surface_from_env() -> Option<ItemSurface> {
    let path = std::env::var(ITEM_SURFACE_ENV).ok()?;
    if path.is_empty() {
        return None;
    }
    let raw = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist `surface` for a nested forge process and return the file path.
pub fn write_item_surface(path: &Path, surface: &ItemSurface) -> Result<()> {
    let raw = serde_json::to_string_pretty(surface).context("serialize ItemSurface")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(path, raw).with_context(|| format!("write {}", path.display()))
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
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn focused_tab_name_from_layout() -> Option<String> {
    focused_tab_name_from_layout_str(&zellij_dump_layout()?)
}

/// Parse dump-layout for the tab that hosts a pane whose cwd matches `cwd`.
/// Prefers a non-focused match when the focused tab is elsewhere (user browsing).
pub fn origin_tab_from_layout(layout: &str, cwd: &Path) -> Option<String> {
    let cwd_s = cwd.to_string_lossy();
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
        // Exact / Path equality, or suffix only when pane cwd looks path-like
        // (contains a separator) — never basename-only (ambiguous across tabs).
        let path_like = pane_cwd.contains('/') || pane_cwd.contains('\\');
        let hit = pane_cwd == cwd_s
            || PathBuf::from(&pane_cwd) == cwd
            || (path_like
                && (pane_cwd.ends_with(cwd_s.as_ref()) || cwd_s.ends_with(pane_cwd.as_str())));
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
pub fn launch_agent_window(
    ctx: &ResolvedTerminal,
    label: &str,
    script_path: &Path,
    cwd: &Path,
) -> Result<()> {
    let script = script_path.display().to_string();
    let cwd_s = cwd.display().to_string();
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
                        "-c",
                        &cwd_s,
                        &run_cmd,
                    ])
                    .status()
                    .context("spawn tmux new-session")?
            } else {
                let st = Command::new("tmux")
                    .args(["split-window", "-t", target, "-c", &cwd_s, &run_cmd])
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
            launch_zellij_in_origin(ctx.zellij.as_ref(), label, &script, &cwd_s, true)
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
    cwd: &str,
    close_on_exit: bool,
) -> Result<()> {
    let caps = zellij_caps();

    // Best path: open beside the invoking pane without following user focus.
    if caps.near_current_pane {
        let args = zellij_run_near_argv(label, script, cwd, close_on_exit);
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
                let args = zellij_new_pane_tab_id_argv(
                    id,
                    label,
                    script,
                    cwd,
                    close_on_exit,
                    caps.no_focus,
                );
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

    // Legacy: always hold the focus lock — never unguarded `zellij run`.
    let _g = focus_guard();
    if let Some(a) = anchor {
        run_zellij_argv(&zellij_goto_argv(&a.tab_name)).context("zellij go-to-tab-name origin")?;
        run_zellij_argv(&zellij_run_argv(label, script, cwd, close_on_exit)).context("zellij run")?;
        if let Some(prev) = &a.restore_tab_name {
            if prev != &a.tab_name {
                let _ = run_zellij_argv(&zellij_goto_argv(prev));
            }
        }
    } else {
        run_zellij_argv(&zellij_run_argv(label, script, cwd, close_on_exit)).context("zellij run")?;
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
            open_zellij_tab(key, &cwd).context("zellij new-tab")?;
            let tab_id = if caps.tab_id || caps.current_tab_info {
                focused_tab_id_after_open()
            } else {
                None
            };
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

/// Read the focused tab id after we just opened a tab (no second new-tab).
fn focused_tab_id_after_open() -> Option<u32> {
    let mut args = zellij_session_args();
    args.extend(["action".into(), "current-tab-info".into()]);
    let out = Command::new("zellij").args(&args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("id:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Launch `bash <script_path>` as a new pane/tab named `role` inside `surface`.
/// `close_on_exit=false` keeps the pane after a clean exit (dry mode).
pub fn launch_agent_in_surface(
    surface: &ItemSurface,
    role: &str,
    script_path: &Path,
    cwd: &Path,
    close_on_exit: bool,
) -> Result<()> {
    let script = script_path.display().to_string();
    let cwd_s = cwd.display().to_string();
    let run_cmd = format!("bash '{script}'");
    match surface {
        ItemSurface::Tmux { session } => {
            run_argv(
                "tmux",
                &tmux_launch_argv(session, &cwd_s, &run_cmd),
            )
            .context("tmux split-window")?;
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
                        &cwd_s,
                        close_on_exit,
                        caps.no_focus,
                    );
                    return run_zellij_argv(&args).context("zellij new-pane --tab-id");
                }
            }
            let _g = focus_guard();
            run_zellij_argv(&zellij_goto_argv(tab)).context("zellij go-to-tab-name")?;
            run_zellij_argv(&zellij_run_argv(role, &script, &cwd_s, close_on_exit))
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
                return launch_agent_window(&fallback, role, script_path, cwd);
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
            // Without --tab-id, close-tab hits the *focused* tab — refuse if focus
            // did not land on the intended name (avoids closing the last/wrong tab
            // and exiting the whole session).
            if let Some(focused) = focused_tab_name_from_layout() {
                if focused != *tab {
                    bail!(
                        "zellij close-tab aborted: focused tab `{focused}` != target `{tab}`"
                    );
                }
            }
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

/// Close every pane in the current tmux window / zellij tab except the focused one.
/// Returns how many panes were closed. No-op (Ok(0)) outside tmux/zellij.
pub fn close_sibling_panes() -> Result<u32> {
    match detect_terminal() {
        Some(TerminalContext::Tmux) => close_tmux_sibling_panes(),
        Some(TerminalContext::Zellij) => close_zellij_sibling_panes(),
        _ => Ok(0),
    }
}

fn close_tmux_sibling_panes() -> Result<u32> {
    let current = match std::env::var("TMUX_PANE") {
        Ok(p) if !p.is_empty() => p,
        _ => return Ok(0),
    };
    let out = Command::new("tmux")
        .args(["list-panes", "-F", "#{pane_id}"])
        .output()
        .context("tmux list-panes")?;
    if !out.status.success() {
        bail!(
            "tmux list-panes failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut closed = 0u32;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let pane = line.trim();
        if pane.is_empty() || pane == current {
            continue;
        }
        let status = Command::new("tmux")
            .args(["kill-pane", "-t", pane])
            .status()
            .with_context(|| format!("tmux kill-pane -t {pane}"))?;
        if status.success() {
            closed += 1;
        }
    }
    Ok(closed)
}

fn close_zellij_sibling_panes() -> Result<u32> {
    // Prefer list-panes --json: close other *terminal* panes in the same tab by
    // id — never focus-next across the session (that wiped Main / Scrutiny).
    if zellij_caps().list_panes {
        if let Some(raw) = zellij_list_panes_json() {
            if let Some(ids) = sibling_terminal_pane_ids_from_json(&raw) {
                let mut closed = 0u32;
                for id in ids {
                    let pane = format!("terminal_{id}");
                    run_zellij_argv(&[
                        "action".into(),
                        "close-pane".into(),
                        "--pane-id".into(),
                        pane,
                    ])
                    .with_context(|| format!("zellij close-pane --pane-id terminal_{id}"))?;
                    closed += 1;
                }
                return Ok(closed);
            }
        }
    }

    // Legacy fallback: focused-tab dump-layout count + focus-next/close.
    let layout = match zellij_dump_layout() {
        Some(l) => l,
        None => return Ok(0),
    };
    let Some(initial) = pane_count_in_focused_tab(&layout) else {
        return Ok(0);
    };
    if initial <= 1 {
        return Ok(0);
    }
    let origin_tab = focused_tab_name_from_layout_str(&layout);
    let mut closed = 0u32;
    let max_close = (initial - 1).min(32);
    for _ in 0..max_close {
        let layout = match zellij_dump_layout() {
            Some(l) => l,
            None => break,
        };
        if let (Some(want), Some(got)) = (&origin_tab, focused_tab_name_from_layout_str(&layout)) {
            if got != *want {
                bail!(
                    "zellij sibling close aborted: focused tab `{got}` != start `{want}` \
                     (refusing to close panes in other tabs)"
                );
            }
        }
        let n = pane_count_in_focused_tab(&layout).unwrap_or(1);
        if n <= 1 {
            break;
        }
        run_zellij_argv(&["action".into(), "focus-next-pane".into()])
            .context("zellij focus-next-pane")?;
        run_zellij_argv(&["action".into(), "close-pane".into()]).context("zellij close-pane")?;
        closed += 1;
    }
    Ok(closed)
}

fn zellij_list_panes_json() -> Option<String> {
    let mut args = zellij_session_args();
    args.extend(["action".into(), "list-panes".into(), "--json".into()]);
    let out = Command::new("zellij").args(&args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Non-plugin, non-floating terminal pane ids in the focused terminal's tab,
/// excluding the focused pane itself.
fn sibling_terminal_pane_ids_from_json(raw: &str) -> Option<Vec<u64>> {
    let panes: Vec<serde_json::Value> = serde_json::from_str(raw).ok()?;
    let focused = panes.iter().find(|p| {
        p.get("is_plugin").and_then(|v| v.as_bool()) == Some(false)
            && p.get("is_floating").and_then(|v| v.as_bool()) != Some(true)
            && p.get("is_focused").and_then(|v| v.as_bool()) == Some(true)
    })?;
    let tab_id = focused.get("tab_id")?;
    let focused_id = focused.get("id").and_then(|v| v.as_u64())?;
    let mut ids: Vec<u64> = panes
        .iter()
        .filter(|p| {
            p.get("tab_id") == Some(tab_id)
                && p.get("is_plugin").and_then(|v| v.as_bool()) == Some(false)
                && p.get("is_floating").and_then(|v| v.as_bool()) != Some(true)
                && p.get("id").and_then(|v| v.as_u64()) != Some(focused_id)
        })
        .filter_map(|p| p.get("id").and_then(|v| v.as_u64()))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    Some(ids)
}

/// Panes inside the dump-layout tab marked `focus=true` only (not the whole session).
fn pane_count_in_focused_tab(layout: &str) -> Option<usize> {
    let mut in_focused = false;
    let mut count = 0usize;
    let mut saw_focused_tab = false;
    for line in layout.lines() {
        let t = line.trim();
        if t.starts_with("tab ") {
            in_focused = t.contains("focus=true");
            if in_focused {
                saw_focused_tab = true;
            }
            continue;
        }
        if !in_focused {
            continue;
        }
        if is_layout_pane_line(t) {
            count += 1;
        }
    }
    if !saw_focused_tab {
        return None;
    }
    Some(count)
}

fn is_layout_pane_line(t: &str) -> bool {
    // KDL dump: `pane …` / bare `pane` / `pane{`. Not `tab` / `panel` / attrs.
    let t = t.trim_start();
    let Some(rest) = t.strip_prefix("pane") else {
        return t.starts_with("PaneId");
    };
    rest.is_empty()
        || rest.starts_with(|c: char| c.is_whitespace() || c == '{' || c == '=')
}

fn focused_tab_name_from_layout_str(layout: &str) -> Option<String> {
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

/// Close the current multiplexer tab / window / session when no [`ItemSurface`] is known.
pub fn close_current_mux_container() -> Result<()> {
    match detect_terminal() {
        Some(TerminalContext::Tmux) => {
            // Forge items use a dedicated session; kill-window is enough for one window.
            // If this is the last window, the session exits too.
            run_argv("tmux", &["kill-window".into()]).context("tmux kill-window")
        }
        Some(TerminalContext::Zellij) => {
            run_zellij_argv(&["action".into(), "close-tab".into()]).context("zellij close-tab")
        }
        Some(TerminalContext::ITerm2) => run_argv(
            "osascript",
            &[
                "-e".to_string(),
                "tell application \"iTerm\" to close (current window)".into(),
            ],
        )
        .context("iTerm2 close current window"),
        Some(TerminalContext::AppleTerminal) => run_argv(
            "osascript",
            &[
                "-e".to_string(),
                "tell application \"Terminal\" to close front window".into(),
            ],
        )
        .context("Terminal.app close front window"),
        None => {
            eprintln!("scrutiny cleanup: not inside tmux/zellij/iTerm/Terminal — skip tab close");
            Ok(())
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

fn tmux_launch_argv(session: &str, cwd: &str, run_cmd: &str) -> Vec<String> {
    ["split-window", "-t", session, "-c", cwd, run_cmd]
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

/// Open a tab whose placeholder pane starts in `cwd`, with session UI chrome
/// (tab bar / status bar) intact.
///
/// Do **not** pass a bare `layout { pane … }` via `--layout`: that creates a
/// chrome-less tab (no tab-bar plugin) that looks like fullscreen. Plain
/// `new-tab --cwd` inherits the session template. Agent panes pin cwd via
/// `zellij run --cwd` — no `write-chars` into the focused PTY (racey / session-toxic).
fn open_zellij_tab(tab: &str, cwd: &str) -> Result<()> {
    run_zellij_argv(&zellij_open_argv(tab, cwd)).context("zellij new-tab")?;
    Ok(())
}

fn zellij_goto_argv(tab: &str) -> Vec<String> {
    ["action", "go-to-tab-name", tab].map(String::from).to_vec()
}

fn zellij_run_argv(role: &str, script: &str, cwd: &str, close_on_exit: bool) -> Vec<String> {
    let mut v = vec!["run".to_string(), "--cwd".into(), cwd.into()];
    if close_on_exit {
        v.push("--close-on-exit".to_string());
    }
    v.extend(["--name", role, "--", "bash", script].map(String::from));
    v
}

pub fn zellij_run_near_argv(role: &str, script: &str, cwd: &str, close_on_exit: bool) -> Vec<String> {
    let mut v = vec![
        "run".to_string(),
        "--near-current-pane".to_string(),
        "--cwd".into(),
        cwd.into(),
    ];
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
    cwd: &str,
    close_on_exit: bool,
    no_focus: bool,
) -> Vec<String> {
    let mut v = vec![
        "action".into(),
        "new-pane".into(),
        "--tab-id".into(),
        tab_id.to_string(),
        "--cwd".into(),
        cwd.into(),
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
         \ttell current session of w to write text \"cd '{cwd}'; clear; echo 'scrutiny forge: {key}'\"\n\
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
         \tset w to do script \"cd '{cwd}'; clear; echo 'scrutiny forge: {key}'\"\n\
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
            tmux_launch_argv("nero-8729", "/tmp/wt", "bash '/tmp/s.sh'"),
            vec![
                "split-window",
                "-t",
                "nero-8729",
                "-c",
                "/tmp/wt",
                "bash '/tmp/s.sh'"
            ]
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
            zellij_run_argv("developer", "/tmp/s.sh", "/tmp/wt", true),
            vec![
                "run",
                "--cwd",
                "/tmp/wt",
                "--close-on-exit",
                "--name",
                "developer",
                "--",
                "bash",
                "/tmp/s.sh"
            ]
        );
        assert_eq!(
            zellij_run_argv("developer", "/tmp/s.sh", "/tmp/wt", false),
            vec![
                "run",
                "--cwd",
                "/tmp/wt",
                "--name",
                "developer",
                "--",
                "bash",
                "/tmp/s.sh"
            ]
        );
    }

    #[test]
    fn zellij_run_near_includes_flag() {
        assert_eq!(
            zellij_run_near_argv("reviewer", "/tmp/s.sh", "/work", true),
            vec![
                "run",
                "--near-current-pane",
                "--cwd",
                "/work",
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
            zellij_new_pane_tab_id_argv(3, "dev", "/tmp/x.sh", "/wt", true, true),
            vec![
                "action",
                "new-pane",
                "--tab-id",
                "3",
                "--cwd",
                "/wt",
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
        // Must stay layout-free so session tab-bar chrome is kept.
        assert!(!zellij_open_argv("PROJ-1", "/tmp/wt").contains(&"--layout".into()));
    }

    #[test]
    fn identity_marker_matches_agent_script() {
        let id = ProcIdentity {
            start: "Tue Sep 15 14:00:00 2026".into(),
            cmd: "bash /tmp/agent-script-abc.sh".into(),
        };
        assert!(identity_matches_marker(&id, "/tmp/agent-script-abc.sh"));
        assert!(!identity_matches_marker(&id, "/tmp/other.sh"));
        assert!(identity_matches_marker(
            &ProcIdentity {
                start: String::new(),
                cmd: "bash /x/agent-script-1.json".into(),
            },
            ""
        ));
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

    #[test]
    fn origin_tab_from_layout_rejects_basename_only() {
        let layout = r#"
layout {
    tab name="Wrong" {
        pane cwd="/elsewhere/scrutiny"
    }
    tab name="Right" {
        pane cwd="/Users/me/dev/scrutiny"
    }
}
"#;
        let origin = origin_tab_from_layout(layout, Path::new("/Users/me/dev/scrutiny"));
        assert_eq!(origin.as_deref(), Some("Right"));
    }

    #[test]
    fn pane_count_in_focused_tab_ignores_other_tabs() {
        let layout = r#"
layout {
    tab name="Main" {
        pane cwd="/a"
        pane cwd="/b"
        pane cwd="/c"
    }
    tab name="Scrutiny" {
        pane cwd="/s1"
        pane cwd="/s2"
    }
    tab name="chore-release" focus=true {
        pane cwd="/wt" focus=true size="50%"
        pane command="agent" cwd="/wt" size="50%"
    }
}
"#;
        assert_eq!(pane_count_in_focused_tab(layout), Some(2));
        assert_eq!(
            focused_tab_name_from_layout_str(layout).as_deref(),
            Some("chore-release")
        );
    }

    #[test]
    fn pane_count_in_focused_tab_none_without_focus() {
        let layout = r#"
layout {
    tab name="Main" {
        pane cwd="/a"
    }
}
"#;
        assert_eq!(pane_count_in_focused_tab(layout), None);
    }

    #[test]
    fn sibling_terminal_pane_ids_same_tab_only() {
        let raw = r#"
[
  {"id": 1, "is_plugin": false, "is_floating": false, "is_focused": false, "tab_id": 0, "tab_name": "Main"},
  {"id": 2, "is_plugin": false, "is_floating": false, "is_focused": false, "tab_id": 0, "tab_name": "Main"},
  {"id": 10, "is_plugin": true, "is_floating": false, "is_focused": false, "tab_id": 1, "tab_name": "task"},
  {"id": 11, "is_plugin": false, "is_floating": false, "is_focused": true, "tab_id": 1, "tab_name": "task"},
  {"id": 12, "is_plugin": false, "is_floating": false, "is_focused": false, "tab_id": 1, "tab_name": "task"},
  {"id": 13, "is_plugin": false, "is_floating": true, "is_focused": false, "tab_id": 1, "tab_name": "task"},
  {"id": 20, "is_plugin": false, "is_floating": false, "is_focused": false, "tab_id": 2, "tab_name": "Scrutiny"}
]
"#;
        assert_eq!(sibling_terminal_pane_ids_from_json(raw), Some(vec![12]));
    }
}
