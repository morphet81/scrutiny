//! `scrutiny forge-all` — for each Jira URL: assign → In Progress → worktree →
//! tmux/zellij tab → `scrutiny forge --yes` with config knobs.

use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use crate::config::{ensure_config, find_shipped_default, load_config, ForgeAllConfig};
use crate::forge::fetch::{jira_key_from_url_or_raw, run_forge_fetch, ForgeFetchInput};
use crate::forge::jira_ops::{jira_assign, jira_transition};
use crate::forge::tools::require_acli_jira;
use crate::git;
use crate::paths::{prepare_artifacts, slug};
use crate::runtime::{resolve_client, ResolveClientInput};
use crate::terminal::{
    detect_terminal, launch_agent_in_surface, open_item_surface, resolve_terminal,
    write_item_surface, zellij_needs_serial_launches, ItemSurface, ResolvedTerminal,
    TerminalContext, ITEM_SURFACE_ENV,
};

#[derive(Debug, Clone)]
pub struct ForgeAllInput {
    pub cwd: PathBuf,
    /// Jira browse URLs or keys.
    pub tickets: Vec<String>,
    pub client: Option<String>,
}

struct PreparedItem {
    key: String,
    worktree: PathBuf,
    branch: String,
    surface: Option<ItemSurface>,
    from_json: String,
    done_sentinel: PathBuf,
    script_path: PathBuf,
}

pub fn run_forge_all(input: ForgeAllInput) -> Result<Vec<PathBuf>> {
    let cwd = input.cwd.clone();
    prepare_artifacts(&cwd, None, &[])?;

    let shipped = find_shipped_default(&std::env::current_exe().unwrap_or_else(|_| cwd.clone()));
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;
    let fa = cfg.forge.all.clone();

    if input.tickets.is_empty() {
        bail!("forge-all needs at least one Jira URL or key");
    }

    // Fail before any assign / worktree / tab work.
    require_acli_jira()?;

    let client_override = input.client.or_else(|| {
        let c = fa.agent_cli.trim();
        if c.is_empty() {
            None
        } else {
            Some(c.to_string())
        }
    });
    let detected = resolve_client(
        &cfg,
        ResolveClientInput {
            cli_override: client_override,
            skip_prompt: true,
        },
    )?;

    let repo = git::discover_repo(&cwd).context("forge-all needs a git repo")?;
    let parent = resolve_worktree_parent(&repo.root, &fa.worktree_parent_folder)?;

    // Always try to open tmux/zellij tabs (forge-all's point), even when
    // `headless = true` for spawned implement agents.
    let term = resolve_terminal(false, &detected.client, "forge-all")
        .or_else(|| force_multiplexer_terminal());

    let scrutiny_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("scrutiny"));
    let from_json = forge_all_answers_json(&fa, &detected.client)?;

    eprintln!(
        "scrutiny forge-all: {} ticket(s), prefix={}, worktrees under {}",
        input.tickets.len(),
        fa.branch_prefix,
        parent.display()
    );
    if zellij_needs_serial_launches() {
        eprintln!(
            "scrutiny forge-all: zellij lacks --near-current-pane/--tab-id \
             (upgrade to ≥0.44) — launching forge drivers one at a time to avoid session thrash"
        );
    }

    let mut prepared: Vec<PreparedItem> = Vec::new();
    for raw in &input.tickets {
        match prepare_one(
            &cwd,
            &repo.root,
            &parent,
            &fa,
            &detected.client,
            term.as_ref(),
            &from_json,
            &scrutiny_bin,
            raw,
        ) {
            Ok(p) => {
                eprintln!(
                    "  ok  {} → branch {} @ {}",
                    p.key,
                    p.branch,
                    p.worktree.display()
                );
                prepared.push(p);
            }
            Err(e) => {
                eprintln!("  ERR {raw}: {e:#}");
            }
        }
    }

    if prepared.is_empty() {
        bail!("forge-all: no tickets prepared");
    }

    let paths = run_forge_pool(&prepared, &scrutiny_bin, term.is_none())?;
    Ok(paths)
}

#[allow(clippy::too_many_arguments)]
fn prepare_one(
    cwd: &Path,
    repo_root: &Path,
    parent: &Path,
    fa: &ForgeAllConfig,
    client: &str,
    term: Option<&ResolvedTerminal>,
    from_json: &str,
    scrutiny_bin: &Path,
    raw: &str,
) -> Result<PreparedItem> {
    let key = jira_key_from_url_or_raw(raw)?;

    eprintln!("scrutiny forge-all [{key}]: assign → {}", fa.jira_assignee);
    jira_assign(cwd, &key, &fa.jira_assignee)?;

    eprintln!(
        "scrutiny forge-all [{key}]: transition → {}",
        fa.in_progress_status
    );
    jira_transition(cwd, &key, &fa.in_progress_status)?;

    let (ticket, _) = run_forge_fetch(ForgeFetchInput {
        cwd: cwd.to_path_buf(),
        input: Some(key.clone()),
        source: Some("jira".into()),
        inline: false,
        client: Some(client.to_string()),
        title: None,
    })?;

    let branch = all_branch_name(&fa.branch_prefix, &ticket.id);
    let worktree = parent.join(branch.replace('/', "-"));
    if worktree.exists() {
        bail!(
            "worktree path already exists: {} — remove or pick another parent",
            worktree.display()
        );
    }
    let worktree = git::create_worktree(repo_root, &branch, &worktree)
        .with_context(|| format!("create worktree for {key}"))?;

    run_init_commands(&worktree, &fa.init_commands)
        .with_context(|| format!("init-commands in {}", worktree.display()))?;

    let session_root = worktree
        .join(".scrutiny")
        .join(slug(&format!("forge-{key}")));
    std::fs::create_dir_all(&session_root)?;
    let done_sentinel = session_root.join("done");
    let _ = std::fs::remove_file(&done_sentinel);

    let surface = match term {
        Some(ctx) => Some(
            open_item_surface(ctx, &key, &worktree)
                .with_context(|| format!("open tab for {key}"))?,
        ),
        None => None,
    };

    let script_path = session_root.join("forge-all-driver.sh");
    let surface_path = session_root.join("item-surface.json");
    if let Some(ref s) = surface {
        write_item_surface(&surface_path, s)?;
    }
    write_forge_script(
        &script_path,
        scrutiny_bin,
        &worktree,
        &key,
        from_json,
        &done_sentinel,
        surface.as_ref().map(|_| surface_path.as_path()),
    )?;

    Ok(PreparedItem {
        key,
        worktree,
        branch,
        surface,
        from_json: from_json.to_string(),
        done_sentinel,
        script_path,
    })
}

/// Run each `[forge_all].init_commands` entry via `sh -c` with cwd = worktree.
fn run_init_commands(worktree: &Path, commands: &[String]) -> Result<()> {
    for (i, raw) in commands.iter().enumerate() {
        let cmd = raw.trim();
        if cmd.is_empty() {
            continue;
        }
        eprintln!(
            "scrutiny forge-all: init-commands[{i}] in {} — {cmd}",
            worktree.display()
        );
        let status = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(worktree)
            .status()
            .with_context(|| format!("spawn init-commands[{i}]: {cmd}"))?;
        if !status.success() {
            bail!("init-commands[{i}] failed (exit {status}): {cmd}");
        }
    }
    Ok(())
}

fn run_forge_pool(
    items: &[PreparedItem],
    scrutiny_bin: &Path,
    headless: bool,
) -> Result<Vec<PathBuf>> {
    let serial = !headless && zellij_needs_serial_launches();
    let (tx, rx) = mpsc::channel::<(usize, Result<PathBuf>)>();
    let mut paths = vec![PathBuf::new(); items.len()];
    let mut first_err: Option<String> = None;

    if serial {
        // One driver at a time: legacy zellij focus-steals on every pane spawn.
        for (idx, item) in items.iter().enumerate() {
            let bin = scrutiny_bin.to_path_buf();
            let key = item.key.clone();
            let worktree = item.worktree.clone();
            let from_json = item.from_json.clone();
            let done_path = item.done_sentinel.clone();
            let script = item.script_path.clone();
            let surface = item.surface.clone();
            let res = if headless || surface.is_none() {
                run_forge_headless(&bin, &worktree, &key, &from_json, &done_path)
            } else {
                run_forge_in_surface(surface.as_ref().unwrap(), &script, &worktree, &done_path)
            }
            .map(|_| worktree);
            let _ = tx.send((idx, res));
        }
    } else {
        for (idx, item) in items.iter().enumerate() {
            let bin = scrutiny_bin.to_path_buf();
            let tx = tx.clone();
            let key = item.key.clone();
            let worktree = item.worktree.clone();
            let from_json = item.from_json.clone();
            let done_path = item.done_sentinel.clone();
            let script = item.script_path.clone();
            let surface = item.surface.clone();
            std::thread::spawn(move || {
                let res = if headless || surface.is_none() {
                    run_forge_headless(&bin, &worktree, &key, &from_json, &done_path)
                } else {
                    run_forge_in_surface(surface.as_ref().unwrap(), &script, &worktree, &done_path)
                }
                .map(|_| worktree);
                let _ = tx.send((idx, res));
            });
        }
    }
    drop(tx);

    let mut done = 0usize;
    while done < items.len() {
        match rx.recv_timeout(Duration::from_secs(3600)) {
            Ok((idx, Ok(p))) => {
                paths[idx] = p;
                done += 1;
                eprintln!(
                    "scrutiny forge-all: finished {} ({}/{})",
                    items[idx].key,
                    done,
                    items.len()
                );
            }
            Ok((idx, Err(e))) => {
                done += 1;
                let msg = format!("{}: {e:#}", items[idx].key);
                eprintln!("scrutiny forge-all: FAIL {msg}");
                if first_err.is_none() {
                    first_err = Some(msg);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("forge-all: timed out waiting for forge workers");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if let Some(e) = first_err {
        bail!("forge-all completed with failures — first: {e}");
    }
    Ok(paths
        .into_iter()
        .filter(|p| !p.as_os_str().is_empty())
        .collect())
}

fn run_forge_headless(
    bin: &Path,
    worktree: &Path,
    key: &str,
    from_json: &str,
    done: &Path,
) -> Result<()> {
    let status = Command::new(bin)
        .arg("forge")
        .arg("--yes")
        .arg("--from-json")
        .arg(from_json)
        .arg("--cwd")
        .arg(worktree)
        .arg(key)
        .current_dir(worktree)
        .status()
        .context("spawn scrutiny forge")?;
    let _ = std::fs::write(done, b"ok\n");
    if !status.success() {
        bail!("scrutiny forge exited {status}");
    }
    Ok(())
}

fn run_forge_in_surface(
    surface: &ItemSurface,
    script: &Path,
    cwd: &Path,
    done: &Path,
) -> Result<()> {
    launch_agent_in_surface(surface, "forge", script, cwd, /* close_on_exit */ false)?;
    // Poll done sentinel (forge script touches it).
    let wall_secs = crate::timeouts::get().forge_bulk_item;
    let wall = Duration::from_secs(wall_secs);
    let start = std::time::Instant::now();
    while start.elapsed() < wall {
        if done.is_file() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!(
        "forge tab did not signal done within {wall_secs}s ({})",
        done.display()
    )
}

fn write_forge_script(
    path: &Path,
    bin: &Path,
    worktree: &Path,
    key: &str,
    from_json: &str,
    done: &Path,
    surface_json: Option<&Path>,
) -> Result<()> {
    // Escape for single-quoted bash strings.
    let esc = |s: &str| s.replace('\'', "'\\''");
    let surface_export = match surface_json {
        Some(p) => format!(
            "export {env}='{path}'\n",
            env = ITEM_SURFACE_ENV,
            path = esc(&p.display().to_string()),
        ),
        None => String::new(),
    };
    let body = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\ncd '{wt}'\n{surface}\
         '{bin}' forge --yes --from-json '{fj}' --cwd '{wt}' '{key}'\n\
         printf 'ok\\n' > '{done}'\n",
        wt = esc(&worktree.display().to_string()),
        surface = surface_export,
        bin = esc(&bin.display().to_string()),
        fj = esc(from_json),
        key = esc(key),
        done = esc(&done.display().to_string()),
    );
    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn forge_all_answers_json(fa: &ForgeAllConfig, resolved_client: &str) -> Result<String> {
    let client = if fa.agent_cli.trim().is_empty() {
        resolved_client
    } else {
        fa.agent_cli.trim()
    };
    let spawn = match fa.spawn_mode.trim().to_ascii_lowercase().as_str() {
        "team" => "team",
        _ => "single",
    };
    let v = json!({
        "client": client,
        "model": fa.model,
        "spawn_mode": spawn,
        "use_playwright": false,
        "tdd": fa.use_tdd,
        "coverage_pct": fa.test_coverage,
        "e2e": fa.require_e2e,
        "agents": fa.team_size,
        "testers": 1,
        "reviewers": 0,
        "evangelists": 0,
    });
    Ok(serde_json::to_string(&v)?)
}

fn all_branch_name(prefix: &str, ticket_id: &str) -> String {
    // Prefer bulk-style key-number stem when id looks like KEY-NUM.
    let prefix = prefix.trim();
    let id = ticket_id.trim();
    let stem = if let Some((key, num)) = id.rsplit_once('-') {
        if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) && !key.is_empty() {
            format!("{}-{}", slug(key).to_ascii_lowercase(), num)
        } else {
            slug(id).to_ascii_lowercase()
        }
    } else {
        slug(id).to_ascii_lowercase()
    };
    join_branch_prefix(prefix, &stem)
}

/// Keep slash (or trailing `-`) in the prefix as written; otherwise insert `-`.
fn join_branch_prefix(prefix: &str, stem: &str) -> String {
    if prefix.is_empty() {
        return stem.to_string();
    }
    if prefix.ends_with('/') || prefix.ends_with('-') {
        format!("{prefix}{stem}")
    } else {
        format!("{prefix}-{stem}")
    }
}

fn resolve_worktree_parent(repo_root: &Path, configured: &str) -> Result<PathBuf> {
    let raw = configured.trim();
    let p = if raw.is_empty() {
        PathBuf::from("..")
    } else {
        PathBuf::from(raw)
    };
    let abs = if p.is_absolute() {
        p
    } else {
        repo_root.join(p)
    };
    let canon = abs.canonicalize().unwrap_or(abs);
    std::fs::create_dir_all(&canon)
        .with_context(|| format!("create worktree parent {}", canon.display()))?;
    Ok(canon)
}

/// When `resolve_terminal(false, …)` fails (e.g. unsupported client), still
/// open a multiplexer tab if we are inside tmux/zellij.
fn force_multiplexer_terminal() -> Option<ResolvedTerminal> {
    match detect_terminal()? {
        kind @ (TerminalContext::Tmux | TerminalContext::Zellij) => Some(ResolvedTerminal {
            kind,
            zellij: None,
            tmux: None,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ForgeAllConfig;

    #[test]
    fn all_branch_name_jira() {
        assert_eq!(all_branch_name("feat", "NERO-123"), "feat-nero-123");
        assert_eq!(all_branch_name("fix/", "ABC-9"), "fix/abc-9");
        assert_eq!(
            all_branch_name("new-tc-manager/", "NERO-697"),
            "new-tc-manager/nero-697"
        );
        assert_eq!(all_branch_name("feat-", "NERO-1"), "feat-nero-1");
    }

    #[test]
    fn answers_json_uses_config_knobs() {
        let fa = ForgeAllConfig {
            use_tdd: false,
            test_coverage: 80,
            require_e2e: false,
            team_size: 4,
            spawn_mode: "team".into(),
            agent_cli: "cursor".into(),
            model: "composer".into(),
            ..ForgeAllConfig::default()
        };
        let raw = forge_all_answers_json(&fa, "claude").unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["client"], "cursor");
        assert_eq!(v["tdd"], false);
        assert_eq!(v["coverage_pct"], 80);
        assert_eq!(v["e2e"], false);
        assert_eq!(v["agents"], 4);
        assert_eq!(v["spawn_mode"], "team");
        assert_eq!(v["model"], "composer");
    }

    #[test]
    fn init_commands_run_in_worktree() {
        let dir = tempfile::tempdir().unwrap();
        run_init_commands(
            dir.path(),
            &["touch init-ok".into(), "test -f init-ok".into()],
        )
        .unwrap();
        assert!(dir.path().join("init-ok").is_file());
    }

    #[test]
    fn init_commands_fail_fast() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_init_commands(dir.path(), &["false".into()]).unwrap_err();
        assert!(
            err.to_string().contains("init-commands[0] failed"),
            "{err:#}"
        );
    }

    #[test]
    fn forge_script_runs_forge_yes() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("driver.sh");
        let surface = dir.path().join("item-surface.json");
        write_forge_script(
            &script,
            Path::new("/bin/scrutiny"),
            Path::new("/tmp/wt"),
            "NERO-1",
            "{}",
            Path::new("/tmp/done"),
            Some(&surface),
        )
        .unwrap();
        let body = std::fs::read_to_string(&script).unwrap();
        assert!(
            !body.contains("SCRUTINY_FORCE_HEADLESS"),
            "forge-all must not force headless (custom models need visible/non-first-output path)"
        );
        assert!(body.contains("forge --yes --from-json"));
        assert!(
            body.contains(ITEM_SURFACE_ENV),
            "driver must export item surface for nested forge: {body}"
        );
    }
}
