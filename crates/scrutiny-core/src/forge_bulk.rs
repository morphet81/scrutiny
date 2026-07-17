//! `scrutiny forge bulk` — collect several tickets, run each on its own branch +
//! worktree (one child driver process per item), and serialize the interactive
//! commit/PR conclude on the main terminal.

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Mutex};
use std::time::Duration;

use crate::agent_runner::{wait_for_sentinels, AGENT_WALL_SECS};
use crate::config::{ensure_config, find_shipped_default, load_config, Config};
use crate::forge::fetch::{run_forge_fetch, ForgeFetchInput, TicketReport};
use crate::forge::scaffold;
use crate::forge_cmd::{
    prompt_forge_answers, run_forge_item_body, run_forge_ship, worktree_dir, ForgeAnswers,
    ForgeItemCtx,
};
use crate::git;
use crate::paths::{init_artifact_ctx, prepare_artifacts, session_name, slug, write_json_pretty};
use crate::runtime::{resolve_client, ResolveClientInput};
use crate::terminal::{
    launch_agent_in_surface, open_item_surface, resolve_terminal, ItemSurface,
};

/// Whole-item wall clock (body drives its own per-agent sub-waits).
const BULK_ITEM_WALL_SECS: u64 = AGENT_WALL_SECS * 8;

#[derive(Debug, Clone)]
pub struct ForgeBulkInput {
    pub cwd: PathBuf,
    pub client: Option<String>,
    pub source: Option<String>,
    /// `--yes`: no menus; read newline-separated keys from stdin, auto params.
    pub non_interactive: bool,
    /// `--concurrency N` override (else config `forge.bulk_concurrency`).
    pub concurrency: Option<usize>,
    /// `--dry`: create branches/worktrees + panes but spawn no agents, no PR.
    pub dry: bool,
}

/// Per-item plan handed to the child driver (`forge-bulk-item --item <this>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ItemPlan {
    id: String,
    session: String,
    ticket_path: PathBuf,
    session_root: PathBuf,
    worktree: PathBuf,
    branch: String,
    prefix: String,
    answers: ForgeAnswers,
    pr_meta_path: PathBuf,
    done_sentinel: PathBuf,
    item_json: PathBuf,
    surface: Option<ItemSurface>,
    client: String,
}

pub fn run_forge_bulk(input: ForgeBulkInput) -> Result<Vec<PathBuf>> {
    let cwd = input.cwd.clone();
    prepare_artifacts(&cwd, None, &[])?;

    let shipped = find_shipped_default(&std::env::current_exe().unwrap_or_else(|_| cwd.clone()));
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;

    let detected = resolve_client(
        &cfg,
        ResolveClientInput {
            cli_override: input.client.clone(),
            skip_prompt: input.non_interactive,
        },
    )?;

    // Stage 1 — collect ticket tokens.
    let tokens = collect_tokens(input.non_interactive)?;
    if tokens.is_empty() {
        eprintln!("scrutiny forge bulk: no tickets — nothing to do");
        return Ok(vec![]);
    }

    // Stage 2 — fetch + complexity (serial; global artifact churn harmless here).
    eprintln!("scrutiny forge bulk: validating {} ticket(s)…", tokens.len());
    let mut tickets: Vec<TicketReport> = Vec::new();
    for tok in &tokens {
        match run_forge_fetch(ForgeFetchInput {
            cwd: cwd.clone(),
            input: Some(tok.clone()),
            source: input.source.clone(),
            inline: false,
            client: Some(detected.client.clone()),
            title: None,
        }) {
            Ok((t, _p)) => {
                eprintln!(
                    "  ok  {:<16} {}  [tier {:?} · {}]",
                    t.id, t.title, t.suggested_forge.tier, t.suggested_forge.complexity_reason
                );
                tickets.push(t);
            }
            Err(e) => eprintln!("  ERR {tok:<16} {e}"),
        }
    }
    if tickets.is_empty() {
        bail!("no valid tickets among {} input(s)", tokens.len());
    }

    // Stage 3 — git repo mandatory (per-item worktrees).
    let repo = git::discover_repo(&cwd)
        .context("bulk needs a git repo for per-item worktrees")?;

    // Stage 4 — params: same-for-all or per-item.
    let per_answers = resolve_bulk_answers(&detected.client, &tickets, input.non_interactive)?;

    // Stage 5/5b — branch + worktree + surface + plan per item (serial).
    let term = resolve_terminal(cfg.headless, &detected.client, "forge-bulk");
    let headless = term.is_none();
    let mut items: Vec<ItemPlan> = Vec::new();
    for (i, ticket) in tickets.iter().enumerate() {
        let prefix = scaffold::guess_prefix(ticket).to_string();
        let branch = scaffold::bulk_branch_name(ticket, &prefix, &repo.repo_slug);
        let dir = worktree_dir(&repo, &branch);
        let worktree = git::create_worktree(&repo.root, &branch, &dir)
            .with_context(|| format!("create worktree for {}", ticket.id))?;
        let session = session_name(None, Some(&ticket.id));
        let session_root = worktree.join(".scrutiny").join(slug(&session));
        std::fs::create_dir_all(&session_root)
            .with_context(|| format!("create {}", session_root.display()))?;
        let ticket_path = session_root.join("ticket.json");
        write_json_pretty(&ticket_path, ticket)?;

        let surface = match term {
            Some(ctx) => Some(
                open_item_surface(ctx, &ticket.id, &worktree)
                    .with_context(|| format!("open surface for {}", ticket.id))?,
            ),
            None => None,
        };

        let item_json = session_root.join("item.json");
        let plan = ItemPlan {
            id: ticket.id.clone(),
            session,
            ticket_path,
            session_root: session_root.clone(),
            worktree,
            branch,
            prefix,
            answers: per_answers[i].clone(),
            pr_meta_path: session_root.join("pr.json"),
            done_sentinel: session_root.join("done"),
            item_json: item_json.clone(),
            surface,
            client: detected.client.clone(),
        };
        write_json_pretty(&item_json, &plan)?;
        items.push(plan);
    }

    // Stage 6 — launch drivers (capped) + serialized conclude.
    let cap = input
        .concurrency
        .unwrap_or(cfg.forge.bulk_concurrency)
        .max(1);
    let scrutiny_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("scrutiny"));
    let conclude = Mutex::new(());
    let dry = input.dry;
    let cfg_ref = &cfg;
    let bin = scrutiny_bin.as_path();
    let conclude_ref = &conclude;

    let results = run_pool(items.clone(), cap, |item| {
        let res = run_one_item(&item, cfg_ref, bin, headless, dry, conclude_ref)
            .map_err(|e| e.to_string());
        (item.id.clone(), res)
    });

    let mut sessions: Vec<PathBuf> = Vec::new();
    eprintln!("\nscrutiny forge bulk: results");
    for (id, res) in results {
        match res {
            Ok(p) => {
                eprintln!("  ok  {id}");
                sessions.push(p);
            }
            Err(e) => eprintln!("  ERR {id}: {e}"),
        }
    }

    // Stage 7 — dry cleanup.
    if dry {
        let del = if input.non_interactive {
            false
        } else {
            Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!(
                    "Delete the {} branch(es) + worktree(s) created by this dry run?",
                    items.len()
                ))
                .default(false)
                .interact()
                .unwrap_or(false)
        };
        if del {
            for it in &items {
                if let Err(e) = git::remove_worktree(&repo.root, &it.worktree) {
                    eprintln!("  worktree {}: {e}", it.branch);
                }
                if let Err(e) = git::delete_branch(&repo.root, &it.branch) {
                    eprintln!("  branch {}: {e}", it.branch);
                }
            }
            eprintln!("scrutiny forge bulk: dry cleanup done");
        } else {
            eprintln!("scrutiny forge bulk: kept dry branches + worktrees");
        }
    }

    Ok(sessions)
}

/// Fan `items` across at most `cap` worker threads, collecting each `work`
/// result. A shared receiver caps in-flight work at exactly `cap.max(1)`.
fn run_pool<T, R, F>(items: Vec<T>, cap: usize, work: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> R + Sync,
{
    let (tx, rx) = mpsc::channel::<T>();
    for it in items {
        tx.send(it).ok();
    }
    drop(tx);
    let rx = Mutex::new(rx);
    let (rtx, rrx) = mpsc::channel::<R>();
    std::thread::scope(|s| {
        for _ in 0..cap.max(1) {
            let worker_tx = rtx.clone();
            let rx = &rx;
            let work = &work;
            s.spawn(move || loop {
                let next = {
                    let g = rx.lock().expect("rx lock");
                    g.recv()
                };
                let item = match next {
                    Ok(i) => i,
                    Err(_) => break,
                };
                let _ = worker_tx.send(work(item));
            });
        }
    });
    drop(rtx);
    rrx.iter().collect()
}

/// Run one item: launch its driver (into its surface, or a captured headless
/// child), wait for done, then hold the conclude mutex to ship serially.
fn run_one_item(
    item: &ItemPlan,
    cfg: &Config,
    scrutiny_bin: &Path,
    headless: bool,
    dry: bool,
    conclude: &Mutex<()>,
) -> Result<PathBuf> {
    let _ = std::fs::remove_file(&item.done_sentinel);

    if headless {
        let mut c = Command::new(scrutiny_bin);
        c.arg("forge-bulk-item")
            .arg("--item")
            .arg(&item.item_json)
            .arg("--headless");
        if dry {
            c.arg("--dry");
        }
        c.current_dir(&item.worktree);
        let status = c.status().context("spawn forge-bulk-item")?;
        if !status.success() {
            eprintln!(
                "scrutiny forge bulk: item {} driver exit {} — using disk state",
                item.id, status
            );
        }
    } else {
        let surface = item
            .surface
            .as_ref()
            .context("non-headless item without a surface")?;
        let script = write_driver_script(scrutiny_bin, item, dry)?;
        launch_agent_in_surface(surface, "driver", &script, /* close_on_exit */ !dry)?;
        let missing = wait_for_sentinels(
            std::slice::from_ref(&item.done_sentinel),
            Duration::from_secs(BULK_ITEM_WALL_SECS),
        );
        if !missing.is_empty() {
            eprintln!(
                "scrutiny forge bulk: item {} did not signal done within {}s — using disk state",
                item.id, BULK_ITEM_WALL_SECS
            );
        }
    }

    // Serialized conclude on the main terminal.
    let _guard = conclude.lock().expect("conclude lock");
    let ticket: TicketReport =
        serde_json::from_str(&std::fs::read_to_string(&item.ticket_path)?)
            .with_context(|| format!("read {}", item.ticket_path.display()))?;

    if dry {
        eprintln!("\n===== [dry] {} — would ship =====", item.id);
        eprintln!("branch:   {}", item.branch);
        eprintln!("worktree: {}", item.worktree.display());
        match std::fs::read_to_string(&item.pr_meta_path) {
            Ok(m) if !m.trim().is_empty() => eprintln!("pr.json:\n{m}"),
            _ => eprintln!("pr.json:  (none)"),
        }
    } else {
        init_artifact_ctx(&item.worktree, &item.session)?;
        run_forge_ship(
            &item.worktree,
            &item.session_root,
            &item.pr_meta_path,
            cfg,
            /* skip_prompts */ headless,
            /* create_pr_noninteractive */ headless,
            &item.prefix,
            &ticket,
        )?;
    }
    drop(_guard);
    Ok(item.session_root.clone())
}

/// Child-driver entry point: run one item's forge body in this process (cwd =
/// worktree), then touch the done-sentinel the orchestrator polls.
pub fn run_forge_bulk_item(plan_path: &Path, headless: bool, dry: bool) -> Result<()> {
    let plan: ItemPlan = serde_json::from_str(
        &std::fs::read_to_string(plan_path)
            .with_context(|| format!("read {}", plan_path.display()))?,
    )
    .with_context(|| format!("parse item plan {}", plan_path.display()))?;

    let worktree = plan.worktree.clone();
    let session_root = init_artifact_ctx(&worktree, &plan.session)?;

    let shipped = find_shipped_default(&std::env::current_exe().unwrap_or_else(|_| worktree.clone()));
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;
    let detected = resolve_client(
        &cfg,
        ResolveClientInput {
            cli_override: Some(plan.client.clone()),
            skip_prompt: true,
        },
    )?;

    let ticket: TicketReport = serde_json::from_str(&std::fs::read_to_string(&plan.ticket_path)?)
        .with_context(|| format!("read {}", plan.ticket_path.display()))?;

    let surface = if headless { None } else { plan.surface.clone() };

    let outcome = run_forge_item_body(ForgeItemCtx {
        detected: &detected,
        cwd: worktree.clone(),
        session_root,
        ticket: &ticket,
        ticket_path: plan.ticket_path.clone(),
        answers: plan.answers.clone(),
        cfg: &cfg,
        prefix: plan.prefix.clone(),
        term: None,
        surface,
        tdd_interactive: !headless,
        dry,
    })?;

    let _ = std::fs::File::create(&plan.done_sentinel);
    eprintln!(
        "scrutiny forge bulk item: done {} pr_meta={}",
        plan.id,
        outcome.pr_meta_path.display()
    );
    Ok(())
}

/// Launcher script for a non-headless item driver (`cd` worktree, run driver).
fn write_driver_script(scrutiny_bin: &Path, item: &ItemPlan, dry: bool) -> Result<PathBuf> {
    let script_path = crate::paths::artifact_path_unique("bulk-driver");
    let dry_arg = if dry { " --dry" } else { "" };
    let script = format!(
        "#!/usr/bin/env bash\ncd '{wt}'\n\
         '{bin}' forge-bulk-item --item '{item}'{dry_arg}\n\
         code=$?\nif [ \"$code\" -eq 0 ]; then exit 0; fi\n\
         echo \"scrutiny forge bulk: {id} driver failed (exit $code)\"\nexec bash\n",
        wt = item.worktree.display(),
        bin = scrutiny_bin.display(),
        item = item.item_json.display(),
        id = item.id,
    );
    std::fs::write(&script_path, script.as_bytes())
        .with_context(|| format!("write {}", script_path.display()))?;
    Ok(script_path)
}

fn collect_tokens(non_interactive: bool) -> Result<Vec<String>> {
    if non_interactive {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("read stdin")?;
        return Ok(buf
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect());
    }
    let theme = ColorfulTheme::default();
    let mut tokens: Vec<String> = Vec::new();
    loop {
        let sel = Select::with_theme(&theme)
            .with_prompt(format!("Add ticket ({} so far)", tokens.len()))
            .items(&["Paste ticket URL/key", "Done"])
            .default(0)
            .interact()
            .context("bulk collect select")?;
        if sel == 1 {
            break;
        }
        let val: String = Input::with_theme(&theme)
            .with_prompt("Ticket URL/key")
            .interact_text()
            .context("bulk ticket input")?;
        let v = val.trim().to_string();
        if !v.is_empty() {
            tokens.push(v);
        }
    }
    Ok(tokens)
}

fn resolve_bulk_answers(
    client: &str,
    tickets: &[TicketReport],
    non_interactive: bool,
) -> Result<Vec<ForgeAnswers>> {
    if non_interactive {
        return Ok(tickets
            .iter()
            .map(|t| answers_from_suggested(client, t))
            .collect());
    }
    let theme = ColorfulTheme::default();
    let mode = Select::with_theme(&theme)
        .with_prompt("Parameters")
        .items(&["Same for all", "Per-item"])
        .default(0)
        .interact()
        .context("bulk params mode")?;
    if mode == 0 {
        let top = tickets
            .iter()
            .max_by_key(|t| t.suggested_forge.complexity_score)
            .expect("non-empty tickets");
        eprintln!(
            "scrutiny forge bulk: shared params from highest-complexity ticket {}",
            top.id
        );
        let ans = prompt_forge_answers(client, top)?;
        Ok(tickets.iter().map(|_| ans.clone()).collect())
    } else {
        let mut out = Vec::new();
        for t in tickets {
            eprintln!("\n===== {} — {} =====\n", t.id, t.title);
            out.push(prompt_forge_answers(client, t)?);
        }
        Ok(out)
    }
}

fn answers_from_suggested(client: &str, ticket: &TicketReport) -> ForgeAnswers {
    let sug = ticket.suggested_forge.clone();
    ForgeAnswers {
        client: client.to_string(),
        model: sug.model,
        spawn_mode: "single".into(),
        use_playwright: false,
        tdd: true,
        coverage_pct: 100,
        e2e: sug.e2e.unwrap_or(true),
        agents: sug.agents,
        testers: sug.testers,
        reviewers: sug.reviewers,
        evangelists: sug.evangelists,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn pool_caps_concurrency_and_conclude_is_serial() {
        let cap = 3;
        let n = 30usize;
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let conclude = Mutex::new(());
        let concl_live = AtomicUsize::new(0);
        let concl_peak = AtomicUsize::new(0);

        let out = run_pool((0..n).collect(), cap, |i: usize| {
            let cur = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(cur, Ordering::SeqCst);
            for _ in 0..2000 {
                std::hint::spin_loop();
            }
            {
                let _g = conclude.lock().expect("conclude");
                let c = concl_live.fetch_add(1, Ordering::SeqCst) + 1;
                concl_peak.fetch_max(c, Ordering::SeqCst);
                concl_live.fetch_sub(1, Ordering::SeqCst);
            }
            live.fetch_sub(1, Ordering::SeqCst);
            i
        });

        assert_eq!(out.len(), n);
        assert!(
            peak.load(Ordering::SeqCst) <= cap,
            "peak {} exceeded cap {cap}",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(
            concl_peak.load(Ordering::SeqCst),
            1,
            "conclude section ran concurrently"
        );
    }
}
