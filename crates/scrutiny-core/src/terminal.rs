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
/// Origin zellij tab captured at [`resolve_terminal`] for held/exited agent sweep.
static AGENT_ORIGIN_TAB: Mutex<Option<ZellijAnchor>> = Mutex::new(None);

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
///
/// After tracked pidfile kills, also closes **held/exited** agent-pattern panes
/// in the origin zellij tab (where probe/parley started) — not other forge tabs.
pub fn force_close_agent_panes() {
    let panes = match TRACKED_AGENT_PANES.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(_) => return,
    };
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

    match close_held_exited_agent_panes_in_origin_tab() {
        Ok(n) if n > 0 => {
            eprintln!("scrutiny: closed {n} held/exited agent pane(s) in origin tab");
        }
        Ok(_) => {}
        Err(e) => eprintln!("scrutiny: skip origin-tab agent sweep: {e:#}"),
    }
}

/// True when pane command/title looks like a scrutiny/claude/cursor agent pane
/// (not an interactive shell).
pub fn is_agent_pane_text(command: &str, title: &str) -> bool {
    let hay = format!("{command} {title}").to_ascii_lowercase();
    const PATS: &[&str] = &[
        "claude",
        "cursor-agent",
        "/.local/bin/agent",
        "/bin/agent ",
        "/bin/agent\t",
        "scrutiny probe",
        "scrutiny parley",
        "parley-",
        "parley_repair",
        "parley-repair",
        "agent-script",
        "forge-all-driver",
        "bulk-driver",
    ];
    if PATS.iter().any(|p| hay.contains(p)) {
        return true;
    }
    // Bare `agent` binary argv (cursor) without matching a shell.
    let cmd = command.trim().to_ascii_lowercase();
    cmd == "agent"
        || cmd.starts_with("agent ")
        || cmd.ends_with("/agent")
        || cmd.contains("/agent --")
}

fn set_agent_origin_tab(anchor: &ZellijAnchor) {
    if let Ok(mut g) = AGENT_ORIGIN_TAB.lock() {
        *g = Some(anchor.clone());
    }
}

fn agent_origin_tab() -> Option<ZellijAnchor> {
    AGENT_ORIGIN_TAB.lock().ok().and_then(|g| g.clone())
}

/// Close session-wide agent panes (live + held + exited). Keeps interactive shells.
/// Returns how many panes were closed.
pub fn close_agent_panes_session_wide() -> Result<u32> {
    match detect_terminal() {
        Some(TerminalContext::Zellij) => close_zellij_agent_panes(AgentPaneCloseScope::SessionWide),
        Some(TerminalContext::Tmux) => close_tmux_agent_panes_session_wide(),
        _ => {
            bail!("scrutiny cleanup --agents needs tmux or zellij");
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum AgentPaneCloseScope {
    /// All tabs: live + held + exited agent panes.
    SessionWide,
    /// Origin tab only: held/exited agent panes (auto cleanup after probe/parley).
    OriginHeldExited,
}

fn close_held_exited_agent_panes_in_origin_tab() -> Result<u32> {
    if detect_terminal() != Some(TerminalContext::Zellij) {
        return Ok(0);
    }
    if agent_origin_tab().is_none() {
        return Ok(0);
    }
    close_zellij_agent_panes(AgentPaneCloseScope::OriginHeldExited)
}

fn close_zellij_agent_panes(scope: AgentPaneCloseScope) -> Result<u32> {
    let raw = zellij_list_panes_json().ok_or_else(|| {
        anyhow::anyhow!("zellij list-panes --json failed (need zellij ≥0.44)")
    })?;
    let targets = agent_pane_ids_from_json(&raw, scope)?;
    let mut closed = 0u32;
    for id in targets {
        let pane = format!("terminal_{id}");
        match run_zellij_argv(&[
            "action".into(),
            "close-pane".into(),
            "--pane-id".into(),
            pane,
        ]) {
            Ok(()) => closed += 1,
            Err(e) => eprintln!("scrutiny: skip close-pane terminal_{id}: {e:#}"),
        }
    }
    Ok(closed)
}

fn agent_pane_ids_from_json(raw: &str, scope: AgentPaneCloseScope) -> Result<Vec<u64>> {
    let panes: Vec<serde_json::Value> =
        serde_json::from_str(raw).context("parse zellij list-panes JSON")?;
    let origin = agent_origin_tab();
    let mut ids = Vec::new();
    for p in &panes {
        if p.get("is_plugin").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        if p.get("is_floating").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let id = match p.get("id").and_then(|v| v.as_u64()) {
            Some(id) => id,
            None => continue,
        };
        let cmd = p
            .get("terminal_command")
            .and_then(|v| v.as_str())
            .or_else(|| p.get("pane_command").and_then(|v| v.as_str()))
            .unwrap_or("");
        let title = p.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if !is_agent_pane_text(cmd, title) {
            continue;
        }
        let held = p.get("is_held").and_then(|v| v.as_bool()).unwrap_or(false);
        let exited = p.get("exited").and_then(|v| v.as_bool()).unwrap_or(false);
        match scope {
            AgentPaneCloseScope::SessionWide => {}
            AgentPaneCloseScope::OriginHeldExited => {
                if !held && !exited {
                    continue;
                }
                let Some(ref o) = origin else {
                    continue;
                };
                let tab_ok = if let Some(want) = o.tab_id {
                    p.get("tab_id").and_then(|v| v.as_u64()) == Some(u64::from(want))
                } else {
                    p.get("tab_name").and_then(|v| v.as_str()) == Some(o.tab_name.as_str())
                };
                if !tab_ok {
                    continue;
                }
            }
        }
        ids.push(id);
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

fn close_tmux_agent_panes_session_wide() -> Result<u32> {
    let out = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{pane_current_command}\t#{pane_title}",
        ])
        .output()
        .context("tmux list-panes -a")?;
    if !out.status.success() {
        bail!(
            "tmux list-panes failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut closed = 0u32;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.splitn(3, '\t');
        let Some(pane_id) = parts.next() else {
            continue;
        };
        let cmd = parts.next().unwrap_or("");
        let title = parts.next().unwrap_or("");
        if !is_agent_pane_text(cmd, title) {
            continue;
        }
        let status = Command::new("tmux")
            .args(["kill-pane", "-t", pane_id])
            .status()
            .with_context(|| format!("tmux kill-pane -t {pane_id}"))?;
        if status.success() {
            closed += 1;
        }
    }
    Ok(closed)
}

/// Zellij server open-file count when inside a session (`None` otherwise).
pub fn zellij_server_open_file_count() -> Option<u64> {
    let pid = zellij_current_server_pid()?;
    count_process_open_files(pid)
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

/// Optional override for [`zellij_session_args`] (tests / rare cross-session ops).
static ZELLIJ_SESSION_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

/// RAII: push a zellij `--session` override; restore previous on drop.
pub struct ZellijSessionOverrideGuard {
    prev: Option<String>,
}

impl Drop for ZellijSessionOverrideGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = ZELLIJ_SESSION_OVERRIDE.lock() {
            *g = self.prev.take();
        }
    }
}

/// Direct all subsequent zellij CLI calls at `name` until the guard drops.
pub fn push_zellij_session_override(name: &str) -> ZellijSessionOverrideGuard {
    let prev = ZELLIJ_SESSION_OVERRIDE
        .lock()
        .ok()
        .and_then(|mut g| g.replace(name.to_string()));
    ZellijSessionOverrideGuard { prev }
}

/// Fallback FD budget per forge tab when caller does not pass a computed budget.
/// Prefer [`zellij_fds_per_forge_tab`].
const ZELLIJ_FDS_PER_FORGE_TAB_DEFAULT: u64 = 48;
/// Keep this many FDs free under the soft limit.
const ZELLIJ_FD_SAFETY_MARGIN: u64 = 24;
/// Tab-bar plugins + idle placeholder shell.
const ZELLIJ_FD_TAB_OVERHEAD: u64 = 12;
/// Rough FDs per terminal pane (driver / agent) on the zellij server.
const ZELLIJ_FD_PER_PANE: u64 = 8;

/// Marker substring kept in the bail message so [`is_zellij_emfile_limit_error`] works.
const EMFILE_BAIL_MARKER: &str = "near open-file limit (EMFILE)";

/// FD headroom to reserve for one multi-ticket forge tab (placeholder + driver +
/// nested `--here` agent panes).
pub fn zellij_fds_per_forge_tab(agents: u32, testers: u32) -> u64 {
    let panes = 1u64 // driver
        .saturating_add(u64::from(agents.max(1)))
        .saturating_add(u64::from(testers));
    ZELLIJ_FD_TAB_OVERHEAD.saturating_add(panes.saturating_mul(ZELLIJ_FD_PER_PANE))
}

/// FD headroom to reserve for `panes` agent windows in the current tab
/// (probe / parley / single-ticket forge).
pub fn zellij_fds_for_panes(panes: u32) -> u64 {
    u64::from(panes.max(1)).saturating_mul(ZELLIJ_FD_PER_PANE)
}

/// True when [`preflight_zellij_open_files`] failed because the next spawn would
/// push the **server** near EMFILE (safe to stop remaining work).
pub fn is_zellij_emfile_limit_error(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    s.contains(EMFILE_BAIL_MARKER) || s.contains("near open-file limit")
}

/// Refuse opening more agent panes when the zellij server is near EMFILE.
///
/// No-op when `term` is `None` (headless) or not Zellij. Call once before a
/// batch of [`run_nonheadless`] spawns with the full pane count.
pub fn preflight_zellij_agent_panes(
    term: Option<&ResolvedTerminal>,
    tool: &str,
    panes: u32,
) -> Result<()> {
    if panes == 0 || term.is_none() {
        return Ok(());
    }
    if detect_terminal() != Some(TerminalContext::Zellij)
        && term.map(|t| t.kind) != Some(TerminalContext::Zellij)
    {
        return Ok(());
    }
    preflight_zellij_open_files_budget_ex(tool, "pane", panes as usize, zellij_fds_for_panes(1))
}

/// Refuse opening `extra_tabs` when the **current** zellij **server** is near EMFILE.
///
/// Zellij panics on "Too many open files" and restarts the whole session — that
/// is how unrelated tabs/tasks get wiped. Better to stop before opening tabs.
///
/// Compares server FD count to the **server** soft limit (not this process's
/// getrlimit — a pane often has a raised ulimit while the server stays at 256).
pub fn preflight_zellij_open_files(extra_tabs: usize) -> Result<()> {
    preflight_zellij_open_files_budget(extra_tabs, ZELLIJ_FDS_PER_FORGE_TAB_DEFAULT)
}

/// Like [`preflight_zellij_open_files`] with an explicit per-tab FD budget (forge).
pub fn preflight_zellij_open_files_budget(extra_tabs: usize, fds_per_tab: u64) -> Result<()> {
    preflight_zellij_open_files_budget_ex("forge", "tab", extra_tabs, fds_per_tab)
}

/// Shared EMFILE preflight for forge tabs or probe/parley panes.
pub fn preflight_zellij_open_files_budget_ex(
    tool: &str,
    unit: &str, // "tab" | "pane"
    count: usize,
    fds_per_unit: u64,
) -> Result<()> {
    if count == 0 || detect_terminal() != Some(TerminalContext::Zellij) {
        return Ok(());
    }
    let unit_plural = if count == 1 {
        unit.to_string()
    } else {
        format!("{unit}s")
    };
    let pid = zellij_current_server_pid().ok_or_else(|| {
        anyhow::anyhow!(
            "could not find zellij server pid for EMFILE preflight — refuse to open {unit_plural} \
             (set ZELLIJ_SESSION_NAME / run inside the session)"
        )
    })?;
    let used = count_process_open_files(pid).ok_or_else(|| {
        anyhow::anyhow!(
            "could not count open files for zellij server pid={pid} (lsof failed) — \
             refuse to open {unit_plural}"
        )
    })?;
    let (self_soft, _) = process_nofile_soft_limit();
    let launchctl = launchctl_maxfiles_soft();
    let proc_soft = server_nofile_soft_from_proc(pid);
    let rlimit_floor = server_fd_nfiles(pid);
    let (limit, limit_src) =
        effective_server_nofile_limit(used, self_soft, launchctl, proc_soft, rlimit_floor);
    let per = fds_per_unit.max(1);
    let need = (count as u64).saturating_mul(per);
    let free = limit.saturating_sub(used);
    let ceiling = limit.saturating_sub(ZELLIJ_FD_SAFETY_MARGIN);
    let session = zellij_session_name_hint();

    eprintln!(
        "scrutiny {tool}: zellij server pid={pid} — {used} file descriptors open \
         / soft limit {limit} from {limit_src} ({free} free; ~{need} needed for \
         {count} new {unit_plural} @ {per} FDs each). Note: FDs (sockets/pipes/files), \
         NOT tab count."
    );
    if used.saturating_add(need) > ceiling {
        print_zellij_emfile_refusal(ZellijEmfileRefusal {
            tool,
            unit,
            count,
            used,
            limit,
            limit_src,
            free,
            need,
            per,
            session: &session,
        });
        bail!(
            "scrutiny {tool}: refused to open {count} {unit_plural}: {EMFILE_BAIL_MARKER}"
        );
    }
    Ok(())
}

struct ZellijEmfileRefusal<'a> {
    tool: &'a str,
    unit: &'a str,
    count: usize,
    used: u64,
    limit: u64,
    limit_src: &'a str,
    free: u64,
    need: u64,
    per: u64,
    session: &'a str,
}

fn zellij_session_name_hint() -> String {
    std::env::var("ZELLIJ_SESSION_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "$SESSION".into())
}

fn emfile_want_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// Colorful EMFILE refusal: why we stopped + how to free FDs or raise the limit.
fn print_zellij_emfile_refusal(r: ZellijEmfileRefusal<'_>) {
    use console::Style;
    let color = emfile_want_color();
    let bold = if color {
        Style::new().bold()
    } else {
        Style::new()
    };
    let red = if color {
        Style::new().bold().red()
    } else {
        Style::new()
    };
    let yellow = if color {
        Style::new().yellow()
    } else {
        Style::new()
    };
    let cyan = if color {
        Style::new().cyan()
    } else {
        Style::new()
    };
    let dim = if color {
        Style::new().dim()
    } else {
        Style::new()
    };
    let green = if color {
        Style::new().green()
    } else {
        Style::new()
    };

    let unit_plural = if r.count == 1 {
        r.unit.to_string()
    } else {
        format!("{}s", r.unit)
    };
    let bar = "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━";

    eprintln!();
    eprintln!("{}", red.apply_to(bar));
    eprintln!(
        "{}",
        red.apply_to("  Zellij near open-file limit (EMFILE)")
    );
    eprintln!("{}", red.apply_to(bar));
    eprintln!();
    eprintln!(
        "  {} refused to open {} agent {} to avoid crashing this whole zellij session.",
        bold.apply_to(format!("scrutiny {}", r.tool)),
        bold.apply_to(r.count.to_string()),
        unit_plural
    );
    eprintln!();
    eprintln!(
        "  {}  {} used / {} soft limit ({})",
        cyan.apply_to("Server FDs:"),
        bold.apply_to(r.used.to_string()),
        bold.apply_to(r.limit.to_string()),
        dim.apply_to(r.limit_src)
    );
    eprintln!(
        "  {}       {}",
        cyan.apply_to("Free:"),
        bold.apply_to(r.free.to_string())
    );
    eprintln!(
        "  {}       ~{} more ({} FDs × {} {})",
        cyan.apply_to("Need:"),
        bold.apply_to(r.need.to_string()),
        r.per,
        r.count,
        unit_plural
    );
    eprintln!(
        "  {}       These are file descriptors (sockets/pipes/PTYs), {}",
        cyan.apply_to("Note:"),
        yellow.apply_to("NOT tab count")
    );
    eprintln!();
    eprintln!("{}", bold.apply_to("  Fix (pick one):"));
    eprintln!();
    eprintln!(
        "  {} Free FDs without killing the session",
        green.apply_to("1)")
    );
    eprintln!(
        "     • Close leftover agent panes: {}",
        cyan.apply_to("scrutiny cleanup --agents -y")
    );
    eprintln!(
        "     • Close done forge tabs: {}",
        cyan.apply_to("scrutiny cleanup -y")
    );
    eprintln!(
        "     • Re-check: {}",
        dim.apply_to("lsof -p <zellij-server-pid> | wc -l")
    );
    eprintln!();
    eprintln!(
        "  {} Raise the server limit (best if you keep hitting this)",
        green.apply_to("2)")
    );
    eprintln!(
        "     Open a normal Terminal/iTerm {} zellij, then:",
        yellow.apply_to("OUTSIDE")
    );
    eprintln!();
    eprintln!("{}", cyan.apply_to("       ulimit -n 10240"));
    eprintln!(
        "{}",
        cyan.apply_to(format!("       zellij kill-session \"{}\"", r.session))
    );
    eprintln!("{}", cyan.apply_to("       ulimit -n 10240"));
    eprintln!(
        "{}",
        cyan.apply_to(format!("       zellij -s \"{}\"", r.session))
    );
    eprintln!();
    eprintln!(
        "     {}",
        dim.apply_to(
            "Pane ulimit alone does nothing — the server keeps its birth limit."
        )
    );
    eprintln!(
        "     {}",
        dim.apply_to("(launchctl system soft can stay 256.)")
    );
    eprintln!();
    eprintln!(
        "  {}",
        yellow.apply_to("Do NOT run kill-session from inside a zellij pane.")
    );
    eprintln!("{}", red.apply_to(bar));
    eprintln!();
}

/// Pick the soft nofile ceiling that applies to the **zellij server**.
///
/// `proc_soft`: Linux `/proc/<pid>/limits` when available.
///
/// macOS cannot read another process's rlimit. `launchctl` soft (often 256) is
/// a **system default**, not the server's birth ulimit — a pane `ulimit -n`
/// also does not change the server. Using `min(self, launchctl)` until
/// `used > launchctl` is a catch-22: we refuse to open panes, so `used` never
/// crosses 256, even when the server was restarted at 10240.
///
/// `rlimit_floor` is Darwin `pbi_nfiles` (`fd_nfiles` — allocated FD table
/// size). The kernel will not grow that table past the process rlimit, so it
/// is a lower bound. Conservative ceiling: `min(self, max(launchctl, floor))`.
pub(crate) fn effective_server_nofile_limit(
    used: u64,
    self_soft: u64,
    launchctl_soft: Option<u64>,
    proc_soft: Option<u64>,
    rlimit_floor: Option<u64>,
) -> (u64, &'static str) {
    let cap_self = |n: u64| {
        if n >= 1_000_000_000 {
            65_536
        } else {
            n
        }
    };
    if let Some(n) = proc_soft.filter(|&n| n > 0) {
        return (cap_self(n), "proc_limits");
    }
    let floor = rlimit_floor.filter(|&n| n > 0).unwrap_or(0);
    match launchctl_soft {
        Some(l) if l > 0 && used > l => {
            (cap_self(self_soft).max(used), "self_getrlimit(proven_raised)")
        }
        Some(l) if l > 0 && floor > l => {
            // Table grew past launchctl ⇒ server rlimit is at least `floor`.
            let n = cap_self(self_soft).min(floor.max(used));
            (n, "macos_fd_nfiles")
        }
        Some(l) if l > 0 => (cap_self(self_soft).min(l), "min(self,launchctl)"),
        _ => (cap_self(self_soft), "self_getrlimit"),
    }
}

/// Linux: soft Max open files from `/proc/<pid>/limits`.
fn server_nofile_soft_from_proc(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/limits")).ok()?;
    parse_proc_limits_nofile_soft(&text)
}

/// Darwin `proc_bsdinfo.pbi_nfiles` = kernel FD table size (`fd_nfiles`).
/// Lower bound on that process's `RLIMIT_NOFILE` (table cannot grow past it).
fn server_fd_nfiles(pid: u32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        return macos_proc_bsdinfo_nfiles(pid);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "macos")]
fn macos_proc_bsdinfo_nfiles(pid: u32) -> Option<u64> {
    // sys/proc_info.h — `struct proc_bsdinfo` / PROC_PIDTBSDINFO = 3.
    const MAXCOMLEN: usize = 16;
    #[repr(C)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: u32,
        pbi_gid: u32,
        pbi_ruid: u32,
        pbi_rgid: u32,
        pbi_svuid: u32,
        pbi_svgid: u32,
        rfu_1: u32,
        pbi_comm: [u8; MAXCOMLEN],
        pbi_name: [u8; 2 * MAXCOMLEN],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }
    extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: i32,
        ) -> i32;
    }
    const PROC_PIDTBSDINFO: i32 = 3;
    let mut info = unsafe { std::mem::zeroed::<ProcBsdInfo>() };
    let sz = std::mem::size_of::<ProcBsdInfo>() as i32;
    let n = unsafe {
        proc_pidinfo(
            pid as i32,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut ProcBsdInfo as *mut libc::c_void,
            sz,
        )
    };
    if n != sz || info.pbi_nfiles == 0 {
        return None;
    }
    Some(u64::from(info.pbi_nfiles))
}

fn parse_proc_limits_nofile_soft(text: &str) -> Option<u64> {
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("Max open files") {
            continue;
        }
        // "Max open files            1024                 4096                 files"
        let mut parts = line.split_whitespace();
        let _ = parts.next()?; // Max
        let _ = parts.next()?; // open
        let _ = parts.next()?; // files
        let soft = parts.next()?;
        if soft.eq_ignore_ascii_case("unlimited") {
            return Some(65_536);
        }
        return soft.parse().ok();
    }
    None
}

fn zellij_current_server_pid() -> Option<u32> {
    let session = std::env::var("ZELLIJ_SESSION_NAME").ok()?;
    if session.is_empty() {
        return None;
    }
    let out = Command::new("ps")
        .args(["-ax", "-o", "pid=,command="])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let line = line.trim();
        if !line.contains("zellij --server") {
            continue;
        }
        // Server argv ends with the session name (may contain spaces).
        if !(line.ends_with(&session) || line.contains(&format!("/{session}"))) {
            continue;
        }
        let pid = line.split_whitespace().next()?.parse().ok()?;
        return Some(pid);
    }
    None
}

fn count_process_open_files(pid: u32) -> Option<u64> {
    let out = Command::new("lsof")
        .args(["-p", &pid.to_string()])
        .output()
        .ok()?;
    // lsof often exits non-zero on macOS when some FDs are unreadable, but still
    // prints useful rows. Fail only when stdout has no data rows.
    let n = String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .count() as u64;
    if n == 0 {
        return None;
    }
    Some(n)
}

/// Soft RLIMIT_NOFILE for **this** process (input to [`effective_server_nofile_limit`]).
///
/// Do **not** treat launchctl alone as the zellij **server** ceiling — see
/// [`effective_server_nofile_limit`]. Prefer `getrlimit` for self; fall back to
/// launchctl only if getrlimit fails.
fn process_nofile_soft_limit() -> (u64, &'static str) {
    #[cfg(unix)]
    {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 && lim.rlim_cur > 0 {
            let soft = lim.rlim_cur as u64;
            // RLIM_INFINITY is huge; treat as "effectively unlimited" for math.
            if soft >= 1_000_000_000 {
                return (65_536, "getrlimit(infinity→65536)");
            }
            return (soft, "getrlimit");
        }
    }
    if let Some(n) = launchctl_maxfiles_soft() {
        return (n, "launchctl");
    }
    (256, "default")
}

fn launchctl_maxfiles_soft() -> Option<u64> {
    // `launchctl limit maxfiles` → "maxfiles    256            unlimited"
    let out = Command::new("launchctl")
        .args(["limit", "maxfiles"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next()? != "maxfiles" {
            continue;
        }
        return parts.next()?.parse().ok();
    }
    None
}

/// Parse `zellij list-sessions -n` line → session name (`New TC Manager [Created …]`).
#[cfg(test)]
fn zellij_session_name_from_ls_line(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let name = line.split(" [Created").next()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

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
#[allow(dead_code)] // retained for mux callers / future serial paths
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
                    if let Some(ref a) = resolved.zellij {
                        set_agent_origin_tab(a);
                    }
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
    if let Ok(g) = ZELLIJ_SESSION_OVERRIDE.lock() {
        if let Some(s) = g.as_ref().filter(|s| !s.is_empty()) {
            return vec!["--session".into(), s.clone()];
        }
    }
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
        run_zellij_argv(&args).context("zellij run --near-current-pane")?;
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
                run_zellij_argv(&args).context("zellij new-pane --tab-id")?;
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
    // Capture stdout so pane/tab ids (`terminal_12`) do not spam the host TTY.
    let out = zellij_cmd(args)
        .output()
        .context("spawn zellij")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "zellij exited with {}: {}",
            out.status,
            err.trim()
        );
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
            let _g = focus_guard();
            let tab_id = open_zellij_tab(key, &cwd).context("zellij new-tab")?;
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
/// Prefer [`open_zellij_tab`]'s stdout id — `current-tab-info` fails on background
/// sessions with no attached client ("No active tab found for current client").
fn focused_tab_id_after_open() -> Option<u32> {
    let mut args = zellij_session_args();
    args.extend(["action".into(), "current-tab-info".into()]);
    let out = Command::new("zellij").args(&args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_tab_id_from_info(&String::from_utf8_lossy(&out.stdout))
}

fn parse_tab_id_from_info(text: &str) -> Option<u32> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("id:") {
            return rest.trim().parse().ok();
        }
    }
    // Newer zellij may print a bare number (same as new-tab).
    text.lines()
        .find_map(|l| {
            let t = l.trim();
            if t.is_empty() {
                None
            } else {
                t.parse().ok()
            }
        })
}

/// Parse `zellij action new-tab` stdout ("Returns: The created tab's ID as a single number").
fn parse_new_tab_id(stdout: &str) -> Option<u32> {
    stdout
        .lines()
        .find_map(|l| {
            let t = l.trim();
            if t.is_empty() {
                None
            } else {
                t.parse().ok()
            }
        })
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
/// `zellij run --cwd`.
///
/// `--cwd` alone is not enough: interactive shell profiles often `cd` away
/// during startup (same as tmux). After the tab exists we `write-chars` an
/// explicit `cd` into the placeholder pane (targeted by pane id when possible).
///
/// Returns the new tab's stable id when the CLI prints it.
fn open_zellij_tab(tab: &str, cwd: &str) -> Result<Option<u32>> {
    let args = zellij_open_argv(tab, cwd);
    let out = zellij_cmd(&args)
        .output()
        .context("spawn zellij new-tab")?;
    if !out.status.success() {
        bail!(
            "zellij new-tab failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let tab_id = parse_new_tab_id(&stdout).or_else(focused_tab_id_after_open);
    if let Some(id) = tab_id {
        let mut rename = zellij_session_args();
        rename.extend([
            "action".into(),
            "rename-tab".into(),
            "--tab-id".into(),
            id.to_string(),
            tab.into(),
        ]);
        let _ = zellij_cmd(&rename).output();
        // Let the interactive shell finish profile init (often cds to ~ or ~/dev).
        thread::sleep(Duration::from_millis(350));
        if let Err(e) = zellij_cd_placeholder(id, tab, cwd) {
            eprintln!(
                "scrutiny: warn: could not cd placeholder tab `{tab}` to worktree: {e:#}"
            );
        }
    }
    Ok(tab_id)
}

/// Force the placeholder shell in `tab_id` into `cwd` (profile may have left `--cwd`).
fn zellij_cd_placeholder(tab_id: u32, tab_name: &str, cwd: &str) -> Result<()> {
    let cmd = shell_cd_clear_line(cwd);
    if let Some(pane) = zellij_first_terminal_pane_in_tab(tab_id) {
        let mut args = zellij_session_args();
        args.extend([
            "action".into(),
            "write-chars".into(),
            "--pane-id".into(),
            format!("terminal_{pane}"),
            cmd,
        ]);
        return run_zellij_argv(&args).context("zellij write-chars --pane-id cd");
    }
    // Fallback: focus the tab by name, write into focused pane, no pane-id.
    let _g = focus_guard();
    run_zellij_argv(&zellij_goto_argv(tab_name)).context("zellij go-to-tab-name for cd")?;
    let mut args = zellij_session_args();
    args.extend(["action".into(), "write-chars".into(), cmd]);
    run_zellij_argv(&args).context("zellij write-chars cd")
}

fn shell_cd_clear_line(cwd: &str) -> String {
    // Match tmux placeholder: cd + clear so the idle pane shows the worktree.
    let esc = cwd.replace('\'', "'\\''");
    format!("cd '{esc}'; clear\n")
}

/// Lowest-id non-plugin terminal pane in `tab_id` (the placeholder after new-tab).
fn zellij_first_terminal_pane_in_tab(tab_id: u32) -> Option<u64> {
    let raw = zellij_list_panes_json()?;
    let panes: Vec<serde_json::Value> = serde_json::from_str(&raw).ok()?;
    let mut ids: Vec<u64> = panes
        .iter()
        .filter(|p| {
            p.get("tab_id").and_then(|v| v.as_u64()) == Some(u64::from(tab_id))
                && p.get("is_plugin").and_then(|v| v.as_bool()) != Some(true)
                && p.get("is_floating").and_then(|v| v.as_bool()) != Some(true)
        })
        .filter_map(|p| p.get("id").and_then(|v| v.as_u64()))
        .collect();
    ids.sort_unstable();
    ids.into_iter().next()
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
    fn soft_maxfiles_parses_launchctl_line() {
        // Mirror launchctl output shape without spawning.
        let line = "\tmaxfiles    256            unlimited";
        let mut parts = line.split_whitespace();
        assert_eq!(parts.next(), Some("maxfiles"));
        assert_eq!(parts.next().unwrap().parse::<u64>().unwrap(), 256);
    }

    #[test]
    fn effective_server_limit_uses_proc_when_present() {
        let (n, src) = effective_server_nofile_limit(100, 1_000_000, Some(256), Some(10240), None);
        assert_eq!(n, 10240);
        assert_eq!(src, "proc_limits");
    }

    #[test]
    fn effective_server_limit_conservative_when_used_under_launchctl() {
        // Client getrlimit is huge; server likely still at launchctl soft 256.
        let (n, src) = effective_server_nofile_limit(237, 1_048_575, Some(256), None, None);
        assert_eq!(n, 256);
        assert_eq!(src, "min(self,launchctl)");
        let ceiling = n.saturating_sub(ZELLIJ_FD_SAFETY_MARGIN);
        assert!(237 + 48 > ceiling, "must refuse crowded soft-256 server");
    }

    #[test]
    fn effective_server_limit_unlocks_when_used_proves_raised() {
        let (n, src) = effective_server_nofile_limit(300, 10_240, Some(256), None, None);
        assert_eq!(n, 10_240);
        assert_eq!(src, "self_getrlimit(proven_raised)");
    }

    #[test]
    fn effective_server_limit_allows_fresh_session_under_256() {
        let (n, _) = effective_server_nofile_limit(95, 1_048_575, Some(256), None, None);
        assert_eq!(n, 256);
        let ceiling = n.saturating_sub(ZELLIJ_FD_SAFETY_MARGIN);
        let need = zellij_fds_per_forge_tab(2, 1);
        assert!(95 + need <= ceiling, "one tab should fit on fresh soft-256");
    }

    #[test]
    fn effective_server_limit_unlocks_when_fd_table_grew_past_launchctl() {
        // macOS catch-22: used=250 < launchctl 256 so proven_raised never fires,
        // but kernel fd table 512 means server rlimit is at least 512.
        let (n, src) =
            effective_server_nofile_limit(250, 10_240, Some(256), None, Some(512));
        assert_eq!(n, 512);
        assert_eq!(src, "macos_fd_nfiles");
        let ceiling = n.saturating_sub(ZELLIJ_FD_SAFETY_MARGIN);
        assert!(250 + 16 <= ceiling, "two agent panes must fit");
    }

    #[test]
    fn parse_proc_limits_nofile_soft_line() {
        let text = "Limit                     Soft Limit           Hard Limit           Units\n\
Max open files            1024                 4096                 files\n";
        assert_eq!(parse_proc_limits_nofile_soft(text), Some(1024));
    }

    #[test]
    fn zellij_fds_budget_scales_with_agents() {
        let one = zellij_fds_per_forge_tab(1, 0);
        let team = zellij_fds_per_forge_tab(2, 1);
        assert!(team > one);
        assert!(team >= ZELLIJ_FD_TAB_OVERHEAD + 4 * ZELLIJ_FD_PER_PANE);
    }

    #[test]
    fn is_zellij_emfile_limit_error_detects_bail_text() {
        let err = anyhow::anyhow!(
            "scrutiny forge: refused to open 1 tabs: near open-file limit (EMFILE)"
        );
        assert!(is_zellij_emfile_limit_error(&err));
        assert!(!is_zellij_emfile_limit_error(&anyhow::anyhow!("lsof failed")));
    }

    #[test]
    fn zellij_fds_for_panes_scales() {
        assert_eq!(zellij_fds_for_panes(1), ZELLIJ_FD_PER_PANE);
        assert_eq!(zellij_fds_for_panes(4), 4 * ZELLIJ_FD_PER_PANE);
    }

    #[test]
    fn zellij_ls_line_parses_spaced_session_names() {
        assert_eq!(
            zellij_session_name_from_ls_line("New TC Manager [Created 7m 35s ago] (current)"),
            Some("New TC Manager")
        );
        assert_eq!(
            zellij_session_name_from_ls_line("Tools [Created 1day 14h 9m 2s ago]"),
            Some("Tools")
        );
        assert_eq!(zellij_session_name_from_ls_line(""), None);
    }

    #[test]
    fn zellij_session_override_wins_over_empty() {
        let _g = push_zellij_session_override("scrutiny-forge");
        assert_eq!(
            zellij_session_args(),
            vec!["--session".to_string(), "scrutiny-forge".into()]
        );
    }

    #[test]
    fn parse_new_tab_id_from_stdout() {
        assert_eq!(parse_new_tab_id("1\n"), Some(1));
        assert_eq!(parse_new_tab_id("  42  \n"), Some(42));
        assert_eq!(parse_new_tab_id("not-a-number\n"), None);
    }

    #[test]
    fn parse_tab_id_from_info_formats() {
        assert_eq!(parse_tab_id_from_info("name: NERO-1\nid: 7\n"), Some(7));
        assert_eq!(parse_tab_id_from_info("3\n"), Some(3));
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
    fn shell_cd_clear_line_escapes_quotes() {
        assert_eq!(
            shell_cd_clear_line("/tmp/o's"),
            "cd '/tmp/o'\\''s'; clear\n"
        );
        assert_eq!(shell_cd_clear_line("/tmp/wt"), "cd '/tmp/wt'; clear\n");
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

    #[test]
    fn is_agent_pane_text_matches_agents_not_shells() {
        assert!(is_agent_pane_text("claude", ""));
        assert!(is_agent_pane_text(
            "/Users/x/.local/bin/agent --use-system-ca index.js --resume",
            "Stack Scrutiny Probe"
        ));
        assert!(is_agent_pane_text("scrutiny parley -y", ""));
        assert!(is_agent_pane_text("bash /tmp/parley-repair.sh", "parley-repair"));
        assert!(!is_agent_pane_text("/bin/zsh", "Pane #1"));
        assert!(!is_agent_pane_text("/bin/bash", ""));
    }

    #[test]
    fn agent_pane_ids_session_wide_skips_shells_and_plugins() {
        let raw = r#"
[
  {"id": 0, "is_plugin": false, "is_floating": false, "is_held": false, "exited": false,
   "tab_id": 0, "tab_name": "Main", "pane_command": "/bin/zsh", "title": "Pane #1"},
  {"id": 1, "is_plugin": false, "is_floating": false, "is_held": true, "exited": false,
   "tab_id": 0, "tab_name": "Main", "terminal_command": "claude", "title": "claude"},
  {"id": 2, "is_plugin": true, "is_floating": false, "is_held": false, "exited": false,
   "tab_id": 0, "tab_name": "Main", "title": "status"},
  {"id": 3, "is_plugin": false, "is_floating": false, "is_held": false, "exited": false,
   "tab_id": 1, "tab_name": "Scrutiny",
   "terminal_command": "/Users/x/.local/bin/agent --resume", "title": "Probe"}
]
"#;
        let ids = agent_pane_ids_from_json(raw, AgentPaneCloseScope::SessionWide).unwrap();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn agent_pane_ids_origin_held_only() {
        set_agent_origin_tab(&ZellijAnchor {
            tab_name: "Scrutiny".into(),
            tab_id: Some(1),
            restore_tab_name: None,
        });
        let raw = r#"
[
  {"id": 1, "is_plugin": false, "is_floating": false, "is_held": true, "exited": false,
   "tab_id": 0, "tab_name": "Main", "terminal_command": "claude", "title": "claude"},
  {"id": 3, "is_plugin": false, "is_floating": false, "is_held": false, "exited": false,
   "tab_id": 1, "tab_name": "Scrutiny",
   "terminal_command": "/Users/x/.local/bin/agent --resume", "title": "Probe"},
  {"id": 4, "is_plugin": false, "is_floating": false, "is_held": true, "exited": false,
   "tab_id": 1, "tab_name": "Scrutiny",
   "terminal_command": "claude", "title": "claude"}
]
"#;
        let ids = agent_pane_ids_from_json(raw, AgentPaneCloseScope::OriginHeldExited).unwrap();
        assert_eq!(ids, vec![4]);
    }
}
