//! Herdr GUI control via `herdr-app` / `Herdr.app` (`do` / `ls` / `status`).
//!
//! Detection: `$HC_TERMINAL_ID` inside a Herdr pane. Forge opens tabs; probe /
//! parley split panes beside the origin.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Max panes in one vertical Herdr column before starting a new stack to the right.
pub const HERDR_STACK_MAX: usize = 4;

/// Where to attach the next agent pane relative to an existing pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HerdrSplit {
    Down { relative: String },
    Right { relative: String },
}

impl HerdrSplit {
    pub fn direction(&self) -> &'static str {
        match self {
            HerdrSplit::Down { .. } => "down",
            HerdrSplit::Right { .. } => "right",
        }
    }

    pub fn relative(&self) -> &str {
        match self {
            HerdrSplit::Down { relative } | HerdrSplit::Right { relative } => relative,
        }
    }
}

/// Per-tab cursor: fill a vertical stack (≤[`HERDR_STACK_MAX`]), then open a new
/// column to the right of that stack's top pane.
#[derive(Debug, Clone, Default)]
pub struct HerdrStackCursor {
    /// Last pane opened in the current stack (split target for the next `down`).
    pub last_pane: Option<String>,
    /// First pane of the current vertical stack (split target for a new column).
    pub stack_top: Option<String>,
    /// Agent panes already in the current vertical stack.
    pub stack_len: usize,
}

impl HerdrStackCursor {
    /// Pick split direction for the next agent pane.
    ///
    /// - Empty stack → `down` from `origin` (or last known pane).
    /// - `stack_len` in 1..max → `down` from last pane.
    /// - Full stack → `right` from stack top (new column).
    pub fn next_split(&self, origin: Option<&str>) -> Option<HerdrSplit> {
        if self.stack_len == 0 {
            let rel = self.last_pane.as_deref().or(origin)?;
            return Some(HerdrSplit::Down {
                relative: rel.to_string(),
            });
        }
        if self.stack_len < HERDR_STACK_MAX {
            let rel = self.last_pane.as_deref().or(origin)?;
            return Some(HerdrSplit::Down {
                relative: rel.to_string(),
            });
        }
        let top = self
            .stack_top
            .as_deref()
            .or(self.last_pane.as_deref())
            .or(origin)?;
        Some(HerdrSplit::Right {
            relative: top.to_string(),
        })
    }

    /// Update cursor after a successful open of `new_pane`.
    pub fn record_opened(&mut self, new_pane: String) {
        if self.stack_len == 0 || self.stack_len >= HERDR_STACK_MAX {
            self.stack_top = Some(new_pane.clone());
            self.stack_len = 1;
        } else {
            self.stack_len += 1;
        }
        self.last_pane = Some(new_pane);
    }
}

fn herdr_stacks() -> &'static Mutex<HashMap<String, HerdrStackCursor>> {
    static STACKS: OnceLock<Mutex<HashMap<String, HerdrStackCursor>>> = OnceLock::new();
    STACKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Next split for `tab_key` (usually Herdr tab id), given optional origin pane.
pub fn take_herdr_split(tab_key: &str, origin_pane: Option<&str>) -> Option<HerdrSplit> {
    let mut map = herdr_stacks().lock().ok()?;
    let cursor = map.entry(tab_key.to_string()).or_default();
    cursor.next_split(origin_pane)
}

/// Record that `new_pane` was opened in `tab_key`'s stack.
pub fn record_herdr_split(tab_key: &str, new_pane: String) {
    if let Ok(mut map) = herdr_stacks().lock() {
        map.entry(tab_key.to_string())
            .or_default()
            .record_opened(new_pane);
    }
}

/// Drop all stack cursors (after force-close / session cleanup).
pub fn clear_herdr_stacks() {
    if let Ok(mut map) = herdr_stacks().lock() {
        map.clear();
    }
}

/// Resolve the Herdr control CLI (`herdr-app` on PATH, else `Herdr.app` binary).
pub fn herdr_cli() -> Result<PathBuf> {
    static CACHED: OnceLock<Option<PathBuf>> = OnceLock::new();
    if let Some(Some(p)) = CACHED.get() {
        return Ok(p.clone());
    }
    let found = find_herdr_cli();
    let _ = CACHED.set(found.clone());
    found.ok_or_else(|| {
        anyhow::anyhow!(
            "herdr-app not found (install Herdr, or put herdr-app on PATH / \
             Applications/Herdr.app)"
        )
    })
}

fn find_herdr_cli() -> Option<PathBuf> {
    if let Ok(p) = which("herdr-app") {
        return Some(p);
    }
    let home = std::env::var_os("HOME")?;
    let candidates = [
        PathBuf::from(&home).join("Applications/Herdr.app/Contents/MacOS/Herdr"),
        PathBuf::from("/Applications/Herdr.app/Contents/MacOS/Herdr"),
        PathBuf::from(&home).join(".local/bin/herdr-app"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

fn which(bin: &str) -> Result<PathBuf> {
    let out = Command::new("which")
        .arg(bin)
        .output()
        .context("which")?;
    if !out.status.success() {
        bail!("not found");
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        bail!("empty");
    }
    Ok(PathBuf::from(p))
}

/// Run `herdr-app do <cmd> --k v …`, parse stdout JSON, surface `{ "error" }` as Err.
pub fn herdr_do(cmd: &str, args: &[(&str, String)]) -> Result<Value> {
    let cli = herdr_cli()?;
    let mut c = Command::new(&cli);
    c.arg("do").arg(cmd);
    for (k, v) in args {
        c.arg(format!("--{k}")).arg(v);
    }
    let out = c
        .output()
        .with_context(|| format!("spawn {} do {cmd}", cli.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let raw = if stdout.trim().is_empty() {
        stderr.to_string()
    } else {
        stdout.to_string()
    };
    let val: Value = serde_json::from_str(raw.trim()).with_context(|| {
        format!(
            "herdr-app do {cmd}: expected JSON, got: {}",
            raw.trim().chars().take(200).collect::<String>()
        )
    })?;
    if let Some(err) = val.get("error").and_then(|e| e.as_str()) {
        bail!("herdr-app do {cmd}: {err}");
    }
    if !out.status.success() {
        bail!(
            "herdr-app do {cmd} exited {}: {}",
            out.status,
            raw.trim()
        );
    }
    Ok(val)
}

/// `herdr-app status` — focused space/tab/pane/terminal.
pub fn herdr_status() -> Result<Value> {
    let cli = herdr_cli()?;
    let out = Command::new(&cli)
        .arg("status")
        .output()
        .with_context(|| format!("spawn {} status", cli.display()))?;
    if !out.status.success() {
        bail!(
            "herdr-app status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("parse herdr-app status JSON")
}

/// Full layout tree (`herdr-app ls`).
pub fn herdr_ls() -> Result<Value> {
    let cli = herdr_cli()?;
    let out = Command::new(&cli)
        .arg("ls")
        .output()
        .with_context(|| format!("spawn {} ls", cli.display()))?;
    if !out.status.success() {
        bail!(
            "herdr-app ls failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("parse herdr-app ls JSON")
}

/// JSON argv array for `herdr-app --argv` (`["bash","/path/script"]`).
pub fn herdr_bash_argv(script: &Path) -> String {
    serde_json::to_string(&["bash", &script.display().to_string()]).expect("serialize argv")
}

/// Parse `{ "space", "tab", … }` from `tab.new` (or similar) JSON.
pub fn parse_tab_ids(v: &Value) -> Result<(String, String)> {
    let space = v
        .get("space")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("herdr response missing space id"))?
        .to_string();
    let tab = v
        .get("tab")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("herdr response missing tab id"))?
        .to_string();
    Ok((space, tab))
}

/// Parse `{ "pane", "terminal" }` from `pane.split` / `terminal.open`.
pub fn parse_pane_ids(v: &Value) -> Result<(String, Option<String>)> {
    let pane = v
        .get("pane")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("herdr response missing pane id"))?
        .to_string();
    let terminal = v
        .get("terminal")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok((pane, terminal))
}

/// Origin anchors from `status` (+ optional `ls` for pane when only terminal known).
#[derive(Debug, Clone)]
pub struct HerdrAnchor {
    pub space_id: String,
    pub tab_id: String,
    pub pane_id: Option<String>,
    pub terminal_id: Option<String>,
}

pub fn capture_herdr_anchor() -> Option<HerdrAnchor> {
    let st = herdr_status().ok()?;
    let space_id = st.get("space")?.as_str()?.to_string();
    let tab_id = st.get("tab")?.as_str()?.to_string();
    let mut pane_id = st
        .get("focused_pane")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let terminal_id = std::env::var("HC_TERMINAL_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            st.get("agent")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        });
    if pane_id.is_none() {
        if let Some(tid) = terminal_id.as_deref() {
            pane_id = pane_for_terminal_in_ls(tid);
        }
    }
    Some(HerdrAnchor {
        space_id,
        tab_id,
        pane_id,
        terminal_id,
    })
}

pub fn pane_for_terminal_in_ls(terminal_id: &str) -> Option<String> {
    let ls = herdr_ls().ok()?;
    find_pane_for_terminal(&ls, terminal_id)
}

/// Walk `ls` JSON for a terminal entry whose `terminal` id matches.
///
/// Real shape: `spaces[].tabs[].terminals[] = { terminal, pane, name, app, … }`.
pub fn find_pane_for_terminal(ls: &Value, terminal_id: &str) -> Option<String> {
    walk_find_pane(ls, terminal_id)
}

fn walk_find_pane(v: &Value, terminal_id: &str) -> Option<String> {
    match v {
        Value::Object(map) => {
            let term = map.get("terminal").and_then(|x| x.as_str());
            let pane = map.get("pane").and_then(|x| x.as_str());
            if term == Some(terminal_id) {
                if let Some(p) = pane.filter(|s| !s.is_empty()) {
                    return Some(p.to_string());
                }
            }
            for child in map.values() {
                if let Some(found) = walk_find_pane(child, terminal_id) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => {
            for child in arr {
                if let Some(found) = walk_find_pane(child, terminal_id) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

/// Collect pane ids in the focused tab (from status + ls), excluding `keep`.
pub fn sibling_pane_ids(ls: &Value, tab_id: &str, keep_pane: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(terms) = terminals_for_tab(ls, tab_id) {
        for entry in terms {
            let Some(pane) = entry.get("pane").and_then(|x| x.as_str()) else {
                continue;
            };
            if pane.is_empty() || keep_pane == Some(pane) {
                continue;
            }
            out.push(pane.to_string());
        }
    }
    out
}

pub fn terminals_for_tab<'a>(ls: &'a Value, tab_id: &str) -> Option<&'a Vec<Value>> {
    let spaces = ls.get("spaces")?.as_array()?;
    for sp in spaces {
        let tabs = sp.get("tabs")?.as_array()?;
        for tab in tabs {
            if tab.get("id").and_then(|x| x.as_str()) == Some(tab_id) {
                return tab.get("terminals")?.as_array();
            }
        }
    }
    None
}

/// First pane id listed under `tab_id` in `ls` (placeholder after `tab.new`).
pub fn first_pane_in_tab(ls: &Value, tab_id: &str) -> Option<String> {
    let terms = terminals_for_tab(ls, tab_id)?;
    for entry in terms {
        if let Some(p) = entry.get("pane").and_then(|x| x.as_str()).filter(|s| !s.is_empty())
        {
            return Some(p.to_string());
        }
    }
    None
}

/// Agent-looking panes across the whole layout (name / app heuristics).
pub fn agent_pane_ids_in_ls(ls: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(spaces) = ls.get("spaces").and_then(|x| x.as_array()) else {
        return out;
    };
    for sp in spaces {
        let Some(tabs) = sp.get("tabs").and_then(|x| x.as_array()) else {
            continue;
        };
        for tab in tabs {
            let Some(terms) = tab.get("terminals").and_then(|x| x.as_array()) else {
                continue;
            };
            for entry in terms {
                let Some(pane) = entry.get("pane").and_then(|x| x.as_str()) else {
                    continue;
                };
                if pane.is_empty() {
                    continue;
                }
                let name = entry.get("name").and_then(|x| x.as_str()).unwrap_or("");
                let app = entry.get("app").and_then(|x| x.as_str()).unwrap_or("");
                let status = entry
                    .get("status")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let hay = format!("{name} {app} {status}");
                if crate::terminal::is_agent_pane_text(app, &hay)
                    || crate::terminal::is_agent_pane_text(status, name)
                    || looks_like_scrutiny_agent_name(name)
                {
                    out.push((pane.to_string(), name.to_string()));
                }
            }
        }
    }
    out
}

fn looks_like_scrutiny_agent_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("probe")
        || n.contains("parley")
        || n.contains("forge")
        || n.contains("reviewer")
        || n.contains("consolidator")
        || n.contains("evangelist")
        || n.contains("developer")
        || n.contains("tester")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bash_argv_json() {
        let s = herdr_bash_argv(Path::new("/tmp/agent.sh"));
        assert_eq!(s, r#"["bash","/tmp/agent.sh"]"#);
    }

    #[test]
    fn parse_tab_and_pane() {
        let v = json!({
            "space": "sp1",
            "tab": "tb1",
            "terminal": "t9"
        });
        assert_eq!(parse_tab_ids(&v).unwrap(), ("sp1".into(), "tb1".into()));
        let p = json!({ "pane": "p1", "terminal": "t2" });
        assert_eq!(
            parse_pane_ids(&p).unwrap(),
            ("p1".into(), Some("t2".into()))
        );
    }

    #[test]
    fn find_pane_walks_ls() {
        let ls = json!({
            "spaces": [{
                "id": "sp1",
                "tabs": [{
                    "id": "tb1",
                    "terminals": [{
                        "terminal": "t42",
                        "pane": "pabc",
                        "name": "shell"
                    }]
                }]
            }]
        });
        assert_eq!(
            find_pane_for_terminal(&ls, "t42").as_deref(),
            Some("pabc")
        );
    }

    #[test]
    fn sibling_and_agent_panes_from_ls() {
        let ls = json!({
            "spaces": [{
                "id": "sp1",
                "tabs": [{
                    "id": "tb1",
                    "terminals": [
                        { "terminal": "t1", "pane": "pkeep", "name": null },
                        { "terminal": "t2", "pane": "pagent", "name": "probe-reviewer" },
                        { "terminal": "t3", "pane": "pshell", "name": null }
                    ]
                }]
            }]
        });
        let sibs = sibling_pane_ids(&ls, "tb1", Some("pkeep"));
        assert_eq!(sibs, vec!["pagent".to_string(), "pshell".to_string()]);
        let agents = agent_pane_ids_in_ls(&ls);
        assert_eq!(agents, vec![("pagent".into(), "probe-reviewer".into())]);
    }

    #[test]
    fn stack_fills_down_then_new_column_right() {
        let mut c = HerdrStackCursor::default();
        let origin = "porigin";
        assert_eq!(
            c.next_split(Some(origin)),
            Some(HerdrSplit::Down {
                relative: origin.into()
            })
        );
        c.record_opened("p1".into());
        assert_eq!(c.stack_len, 1);
        assert_eq!(
            c.next_split(Some(origin)),
            Some(HerdrSplit::Down {
                relative: "p1".into()
            })
        );
        c.record_opened("p2".into());
        c.record_opened("p3".into());
        c.record_opened("p4".into());
        assert_eq!(c.stack_len, 4);
        assert_eq!(
            c.next_split(Some(origin)),
            Some(HerdrSplit::Right {
                relative: "p1".into()
            })
        );
        c.record_opened("p5".into());
        assert_eq!(c.stack_len, 1);
        assert_eq!(c.stack_top.as_deref(), Some("p5"));
        assert_eq!(
            c.next_split(Some(origin)),
            Some(HerdrSplit::Down {
                relative: "p5".into()
            })
        );
    }
}
