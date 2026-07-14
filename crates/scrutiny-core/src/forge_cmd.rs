//! End-to-end `scrutiny forge` orchestration.

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::agent_runner::{run_headless, HeadlessKind, AGENT_WALL_SECS};
use crate::config::{ensure_config, find_shipped_default, load_config, Config};
use crate::forge::brief::run_forge_brief;
use crate::forge::context::run_forge_context;
use crate::forge::fetch::{run_forge_fetch, ForgeFetchInput, TicketReport};
use crate::forge::figma::export_figma_designs;
use crate::forge::plan::{run_forge_plan_write, ForgePlanWriteInput, ForgeSessionPlan};
use crate::forge::tools::playwright_cli_available;
use crate::git::{self, git_ok, git_stdout};
use crate::paths::{prepare_artifacts, write_json_pretty};
use crate::runtime::{resolve_client, ResolveClientInput};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrMeta {
    pub pr_title: String,
    pub pr_body: String,
    pub commit_subject: String,
    #[serde(default)]
    pub commit_body: String,
}

#[derive(Debug, Clone)]
pub struct ForgeCmdInput {
    pub cwd: PathBuf,
    pub input: Option<String>,
    pub inline: bool,
    pub source: Option<String>,
    pub client: Option<String>,
    pub title: Option<String>,
    /// Skip menus; full ForgeSessionPlan + knobs as JSON.
    pub from_json: Option<String>,
    pub non_interactive: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ForgeFromJson {
    #[serde(flatten)]
    plan: ForgeAnswers,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ForgeAnswers {
    pub client: String,
    pub model: String,
    #[serde(default = "default_single")]
    pub spawn_mode: String,
    #[serde(default)]
    pub use_playwright: bool,
    #[serde(default = "default_true")]
    pub tdd: bool,
    #[serde(default = "default_coverage")]
    pub coverage_pct: u32,
    #[serde(default = "default_true")]
    pub e2e: bool,
    #[serde(default = "default_agents")]
    pub agents: u32,
    #[serde(default = "default_testers")]
    pub testers: u32,
    #[serde(default)]
    pub reviewers: u32,
    #[serde(default)]
    pub evangelists: u32,
}

fn default_single() -> String {
    "single".into()
}
fn default_true() -> bool {
    true
}
fn default_coverage() -> u32 {
    100
}
fn default_agents() -> u32 {
    2
}
fn default_testers() -> u32 {
    1
}

/// Run forge: fetch → figma → knobs → optional TDD plan → implement agent.
pub fn run_forge(input: ForgeCmdInput) -> Result<PathBuf> {
    let cwd = input.cwd.clone();
    prepare_artifacts(&cwd, None, &[])?;

    let shipped = find_shipped_default(&std::env::current_exe().unwrap_or_else(|_| cwd.clone()));
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;

    let detected = resolve_client(
        &cfg,
        ResolveClientInput {
            cli_override: input.client.clone(),
            skip_prompt: input.non_interactive || input.from_json.is_some(),
        },
    )?;

    eprintln!("scrutiny forge: fetch ticket…");
    let (mut ticket, ticket_path) = run_forge_fetch(ForgeFetchInput {
        cwd: cwd.clone(),
        input: input.input.clone(),
        source: input.source.clone(),
        inline: input.inline,
        client: Some(detected.client.clone()),
        title: input.title.clone(),
    })?;

    let session_root = crate::paths::init_artifact_ctx(
        &cwd,
        &crate::paths::session_name(None, Some(&ticket.id)),
    )?;
    eprintln!(
        "scrutiny forge: ticket {} → {}",
        ticket.id,
        ticket_path.display()
    );

    // Figma
    if !ticket.figma_urls.is_empty() {
        eprintln!(
            "scrutiny forge: {} Figma link(s) — exporting via fcli…",
            ticket.figma_urls.len()
        );
        match export_figma_designs(&cwd, &session_root, &ticket.figma_urls)? {
            Some(rep) => {
                ticket.figma_dir = Some(rep.dir.clone());
                write_json_pretty(&ticket_path, &ticket)?;
                eprintln!("scrutiny forge: figma → {}", rep.dir);
            }
            None => {}
        }
    }

    let answers = if let Some(raw) = &input.from_json {
        let v: ForgeFromJson =
            serde_json::from_str(raw).context("parse forge --from-json")?;
        v.plan
    } else if input.non_interactive {
        let sug = ticket.suggested_forge.clone();
        ForgeAnswers {
            client: detected.client.clone(),
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
    } else {
        prompt_forge_answers(&detected.client, &ticket)?
    };

    let (_session, session_path) = run_forge_plan_write(ForgePlanWriteInput {
        ticket_path: ticket_path.clone(),
        client: answers.client.clone(),
        model: answers.model.clone(),
        approach: if answers.tdd {
            "tdd".into()
        } else {
            "heads_down".into()
        },
        e2e: answers.e2e,
        agents: answers.agents,
        testers: answers.testers,
        reviewers: answers.reviewers,
        evangelists: answers.evangelists,
        cwd: Some(cwd.clone()),
        spawn_mode: answers.spawn_mode.clone(),
        use_playwright: answers.use_playwright,
        coverage_pct: answers.coverage_pct,
        tdd: answers.tdd,
        tdd_plan_path: None,
        figma_dir: ticket.figma_dir.clone(),
    })?;

    let mut session: ForgeSessionPlan = serde_json::from_str(
        &fs::read_to_string(&session_path).context("read session")?,
    )?;

    eprintln!("scrutiny forge: context + brief…");
    let (_ctx, context_path) = run_forge_context(&ticket_path, &cwd)?;
    let (_brief, brief_path) =
        run_forge_brief(&ticket_path, Some(&session_path), Some(&context_path))?;

    if session.tdd {
        let plan_path = run_tdd_plan_loop(
            &detected,
            &session.model,
            &cwd,
            &session_root,
            &ticket_path,
            &session_path,
            &brief_path,
            &context_path,
            &session,
        )?;
        session.tdd_plan_path = Some(plan_path.display().to_string());
        write_json_pretty(&session_path, &session)?;
    }

    eprintln!(
        "scrutiny forge: implement ({})…",
        session.spawn_mode
    );
    let pr_meta_path = session_root.join("pr.json");
    run_implement_agent(
        &detected,
        &session.model,
        &cwd,
        &ticket_path,
        &session_path,
        &brief_path,
        &context_path,
        &session,
        &ticket,
        &pr_meta_path,
    )?;

    let skip_pr_prompt = input.non_interactive || input.from_json.is_some();
    run_forge_ship(&cwd, &session_root, &pr_meta_path, &cfg, skip_pr_prompt)?;

    eprintln!(
        "scrutiny forge: done. session={} ticket={} pr_meta={}",
        session_path.display(),
        ticket_path.display(),
        pr_meta_path.display()
    );
    Ok(session_path)
}

fn prompt_forge_answers(client: &str, ticket: &TicketReport) -> Result<ForgeAnswers> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "forge needs a TTY for knobs (or pass --from-json / --yes).\n\
             Example --from-json: {{\"client\":\"claude\",\"model\":\"sonnet\",\"spawn_mode\":\"single\",\"tdd\":true,\"e2e\":true,\"coverage_pct\":100}}"
        );
    }
    let theme = ColorfulTheme::default();
    let sug = &ticket.suggested_forge;

    let spawn_items = ["single — one agent implements", "team — PO spawns a team"];
    let spawn_sel = Select::with_theme(&theme)
        .with_prompt("Spawn mode")
        .items(&spawn_items)
        .default(0)
        .interact()
        .context("spawn mode")?;
    let spawn_mode = if spawn_sel == 0 {
        "single".into()
    } else {
        "team".into()
    };

    let use_playwright = if playwright_cli_available() {
        Confirm::with_theme(&theme)
            .with_prompt("Should agent use playwright-cli to verify implementation?")
            .default(false)
            .interact()
            .context("playwright confirm")?
    } else {
        false
    };

    let tdd = Confirm::with_theme(&theme)
        .with_prompt("TDD mode?")
        .default(true)
        .interact()
        .context("tdd confirm")?;

    let coverage_pct: u32 = Input::with_theme(&theme)
        .with_prompt("Expected test coverage %")
        .default(100u32)
        .interact_text()
        .context("coverage input")?;

    let e2e = Confirm::with_theme(&theme)
        .with_prompt("Require e2e tests?")
        .default(true)
        .interact()
        .context("e2e confirm")?;

    let model = if sug.prompt_model && !sug.model.is_empty() {
        let models = if sug.model.is_empty() {
            vec![sug.model.clone()]
        } else {
            // offer suggested as default via Confirm skip — keep suggested
            vec![sug.model.clone()]
        };
        let _ = models;
        sug.model.clone()
    } else {
        sug.model.clone()
    };

    Ok(ForgeAnswers {
        client: client.to_string(),
        model,
        spawn_mode,
        use_playwright,
        tdd,
        coverage_pct: coverage_pct.min(100),
        e2e,
        agents: sug.agents.max(1),
        testers: sug.testers,
        reviewers: sug.reviewers,
        evangelists: sug.evangelists,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_tdd_plan_loop(
    client: &crate::runtime::DetectedClient,
    model: &str,
    cwd: &Path,
    session_root: &Path,
    ticket_path: &Path,
    session_path: &Path,
    brief_path: &Path,
    context_path: &Path,
    session: &ForgeSessionPlan,
) -> Result<PathBuf> {
    let plan_path = session_root.join("test-plan.md");
    let theme = ColorfulTheme::default();

    loop {
        let prompt = build_test_plan_prompt(
            ticket_path,
            session_path,
            brief_path,
            context_path,
            session,
            &plan_path,
            None,
        );
        eprintln!("scrutiny forge: generating test plan…");
        let out = run_headless(
            client,
            model,
            cwd,
            &prompt,
            HeadlessKind::Forge,
            "forge-test-plan",
            Duration::from_secs(AGENT_WALL_SECS),
        )?;
        if out.code != 0 && !out.timed_out && !plan_path.exists() {
            bail!(
                "test-plan agent failed: {}",
                out.stderr.chars().take(400).collect::<String>()
            );
        }
        // If agent did not write file, salvage from stdout
        if !plan_path.exists() {
            let text = extract_markdownish(&out.stdout);
            if text.trim().is_empty() {
                bail!("test-plan agent produced no markdown");
            }
            fs::write(&plan_path, text).context("write test-plan.md")?;
        }

        let plan_text = fs::read_to_string(&plan_path).context("read test-plan.md")?;
        eprintln!("\n======== TEST PLAN ========\n{plan_text}\n===========================\n");

        if !std::io::stdin().is_terminal() {
            eprintln!("scrutiny forge: non-TTY — auto-confirm test plan");
            break;
        }

        let sel = Select::with_theme(&theme)
            .with_prompt("Test plan")
            .items(&["Confirm", "Comment (revise)"])
            .default(0)
            .interact()
            .context("test plan menu")?;
        if sel == 0 {
            break;
        }
        let comment: String = Input::with_theme(&theme)
            .with_prompt("Your comments")
            .interact_text()
            .context("test plan comment")?;
        let rev_prompt = build_test_plan_prompt(
            ticket_path,
            session_path,
            brief_path,
            context_path,
            session,
            &plan_path,
            Some(&comment),
        );
        eprintln!("scrutiny forge: revising test plan…");
        let out = run_headless(
            client,
            model,
            cwd,
            &rev_prompt,
            HeadlessKind::Forge,
            "forge-test-plan-revise",
            Duration::from_secs(AGENT_WALL_SECS),
        )?;
        if !plan_path.exists() {
            let text = extract_markdownish(&out.stdout);
            if !text.trim().is_empty() {
                fs::write(&plan_path, text).ok();
            }
        }
    }

    Ok(plan_path)
}

fn build_test_plan_prompt(
    ticket_path: &Path,
    session_path: &Path,
    brief_path: &Path,
    context_path: &Path,
    session: &ForgeSessionPlan,
    plan_path: &Path,
    comment: Option<&str>,
) -> String {
    let mut p = String::new();
    p.push_str("You are a test planner. Do NOT implement production code.\n");
    p.push_str("Read these paths only:\n");
    p.push_str(&format!("- ticket: {}\n", ticket_path.display()));
    p.push_str(&format!("- session: {}\n", session_path.display()));
    p.push_str(&format!("- brief: {}\n", brief_path.display()));
    p.push_str(&format!("- context: {}\n", context_path.display()));
    if let Some(f) = &session.figma_dir {
        p.push_str(&format!("- figma assets: {f}\n"));
    }
    p.push_str(&format!(
        "\nConstraints: e2e_required={} coverage_target={}%\n",
        session.e2e, session.coverage_pct
    ));
    p.push_str(
        "\nWrite a complete test plan that covers ALL requirements / acceptance criteria.\n\
         Include unit cases",
    );
    if session.e2e {
        p.push_str(" and e2e cases");
    }
    p.push_str(
        " with suggested file paths, AC mapping, and edge cases.\n\
         Guidelines: every AC ≥1 test; bugs need regression tests; no locale string asserts \
         (use i18n keys); follow project test layout from context.\n",
    );
    p.push_str(&format!(
        "\nOverwrite this file with the full markdown plan:\n  {}\n",
        plan_path.display()
    ));
    if let Some(c) = comment {
        p.push_str("\nUser requested revisions:\n");
        p.push_str(c);
        p.push('\n');
        p.push_str("Update the plan accordingly; keep what still applies.\n");
    }
    p
}

#[allow(clippy::too_many_arguments)]
fn run_implement_agent(
    client: &crate::runtime::DetectedClient,
    model: &str,
    cwd: &Path,
    ticket_path: &Path,
    session_path: &Path,
    brief_path: &Path,
    context_path: &Path,
    session: &ForgeSessionPlan,
    ticket: &TicketReport,
    pr_meta_path: &Path,
) -> Result<()> {
    let prompt = build_implement_prompt(
        ticket_path,
        session_path,
        brief_path,
        context_path,
        session,
        ticket,
        pr_meta_path,
    );
    let label = if session.spawn_mode == "team" {
        "forge-po-team"
    } else {
        "forge-implement"
    };
    let out = run_headless(
        client,
        model,
        cwd,
        &prompt,
        HeadlessKind::Forge,
        label,
        Duration::from_secs(AGENT_WALL_SECS.saturating_mul(2)),
    )?;
    if out.code != 0 && !out.timed_out {
        eprintln!(
            "scrutiny forge: implement agent exit {} — check output",
            out.code
        );
    }
    Ok(())
}

fn build_implement_prompt(
    ticket_path: &Path,
    session_path: &Path,
    brief_path: &Path,
    context_path: &Path,
    session: &ForgeSessionPlan,
    ticket: &TicketReport,
    pr_meta_path: &Path,
) -> String {
    let mut p = String::new();
    if session.spawn_mode == "team" {
        p.push_str(
            "You are the Product Owner / team lead for this ticket.\n\
             Spawn a team of agents to implement. Wait for members. Merge results.\n\
             Do not invent ticket facts — read local files only.\n",
        );
    } else {
        p.push_str(
            "You implement this ticket yourself (single agent).\n\
             Do not invent ticket facts — read local files only.\n",
        );
    }
    p.push_str("\nRead:\n");
    p.push_str(&format!("- ticket: {}\n", ticket_path.display()));
    p.push_str(&format!("- session: {}\n", session_path.display()));
    p.push_str(&format!("- brief: {}\n", brief_path.display()));
    p.push_str(&format!("- context: {}\n", context_path.display()));
    if let Some(tp) = &session.tdd_plan_path {
        p.push_str(&format!("- approved test plan: {tp}\n"));
    }
    if let Some(f) = &session.figma_dir {
        p.push_str(&format!("- figma (screenshots + structure): {f}\n"));
    }

    p.push_str("\n## User choices (honor exactly)\n");
    p.push_str(&format!("- spawn_mode: {}\n", session.spawn_mode));
    p.push_str(&format!("- tdd: {}\n", session.tdd));
    p.push_str(&format!("- e2e_required: {}\n", session.e2e));
    p.push_str(&format!("- coverage_target: {}%\n", session.coverage_pct));
    p.push_str(&format!("- use_playwright_cli: {}\n", session.use_playwright));

    if session.tdd {
        if session.tdd_plan_path.is_some() {
            p.push_str(
                "\nTDD: implement tests from the approved plan first (red), then production code (green).\n",
            );
        } else {
            p.push_str("\nTDD: write failing tests before production code.\n");
        }
    }
    if session.e2e {
        p.push_str("Include e2e coverage for critical user flows.\n");
    } else {
        p.push_str("Do NOT add e2e tests unless already required by existing suite.\n");
    }
    if session.use_playwright {
        p.push_str(
            "\nVerification: use `playwright-cli` on PATH to exercise UI flows after implementation.\n",
        );
    } else {
        p.push_str("\nDo NOT use playwright-cli.\n");
    }
    p.push_str(&format!(
        "Aim for ~{}% test coverage on changed code.\n",
        session.coverage_pct
    ));

    p.push_str("\n## Shipping metadata (required before you finish)\n");
    p.push_str(&format!(
        "Write JSON exactly to this path (create/overwrite):\n{}\n",
        pr_meta_path.display()
    ));
    p.push_str(
        "Schema:\n\
         {\n\
         \"pr_title\": \"…\",\n\
         \"pr_body\": \"…\",\n\
         \"commit_subject\": \"…\",\n\
         \"commit_body\": \"…\"\n\
         }\n",
    );
    p.push_str(
        "- pr_title: short PR title for this branch.\n\
         - pr_body: PR description based on the ticket. Must reference the ticket URL below \
         (that URL only — do not invent or link a different ticket).\n\
         - commit_subject / commit_body: conventional commit message for one final commit \
         the host script will create.\n",
    );
    match ticket.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        Some(url) => {
            p.push_str(&format!("Ticket URL (cite this in pr_body): {url}\n"));
        }
        None => {
            p.push_str(
                "No ticket URL (inline / missing) — omit ticket links from pr_body.\n",
            );
        }
    }

    p.push_str(
        "\n## Cleanup (required)\n\
         After implementation, delete files that are not part of the product implementation \
         (e.g. playwright-cli screenshots/videos, ad-hoc temp assets created while testing). \
         Keep source changes and forge session artifacts under .scrutiny/.\n",
    );
    p.push_str(
        "\n## Do NOT ship yourself\n\
         Do NOT run git commit, git push, or open a PR. The host script commits and may \
         create a draft PR after you finish.\n",
    );
    p
}

fn load_pr_meta(path: &Path) -> Result<PrMeta> {
    if !path.is_file() {
        bail!(
            "implement agent did not write shipping metadata at {} — expected pr.json with \
             pr_title, pr_body, commit_subject, commit_body",
            path.display()
        );
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let meta: PrMeta = serde_json::from_str(&raw)
        .with_context(|| format!("parse pr.json at {}", path.display()))?;
    if meta.pr_title.trim().is_empty()
        || meta.commit_subject.trim().is_empty()
    {
        bail!(
            "pr.json at {} missing pr_title or commit_subject",
            path.display()
        );
    }
    Ok(meta)
}

fn run_forge_ship(
    cwd: &Path,
    session_root: &Path,
    pr_meta_path: &Path,
    cfg: &Config,
    skip_pr_prompt: bool,
) -> Result<()> {
    let meta = load_pr_meta(pr_meta_path)?;
    eprintln!(
        "scrutiny forge: shipping metadata → {}",
        pr_meta_path.display()
    );

    let status = git_stdout(cwd, &["status", "--porcelain"])?;
    if status.trim().is_empty() {
        eprintln!("scrutiny forge: working tree clean — skip commit");
    } else {
        let add = Command::new("git")
            .args(["add", "-A"])
            .current_dir(cwd)
            .output()
            .context("git add -A")?;
        if !add.status.success() {
            bail!(
                "git add -A failed: {}",
                String::from_utf8_lossy(&add.stderr).trim()
            );
        }

        let mut msg = meta.commit_subject.trim().to_string();
        let body = meta.commit_body.trim();
        if !body.is_empty() {
            msg.push_str("\n\n");
            msg.push_str(body);
            msg.push('\n');
        } else {
            msg.push('\n');
        }
        let msg_path = session_root.join("commit-msg.txt");
        fs::write(&msg_path, &msg)
            .with_context(|| format!("write {}", msg_path.display()))?;

        let commit = Command::new("git")
            .args(["commit", "-F"])
            .arg(&msg_path)
            .current_dir(cwd)
            .output()
            .context("git commit")?;
        if !commit.status.success() {
            bail!(
                "git commit failed: {}",
                String::from_utf8_lossy(&commit.stderr).trim()
            );
        }
        eprintln!(
            "scrutiny forge: committed — {}",
            meta.commit_subject.trim()
        );
    }

    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if skip_pr_prompt || !tty {
        eprintln!(
            "scrutiny forge: skip draft PR prompt (non-interactive). \
             pr.json ready at {}",
            pr_meta_path.display()
        );
        return Ok(());
    }

    let create = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Create draft PR?")
        .default(false)
        .interact()
        .context("draft PR confirm")?;
    if !create {
        eprintln!("scrutiny forge: draft PR skipped");
        return Ok(());
    }

    let default_base = git::resolve_base_branch(cwd, &cfg.git.base_candidates, None)
        .unwrap_or_else(|_| "main".into());
    // Strip origin/ for gh --base (expects branch name)
    let default_base = default_base
        .strip_prefix("origin/")
        .unwrap_or(&default_base)
        .to_string();

    let base: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Base branch")
        .default(default_base)
        .interact_text()
        .context("base branch")?;
    let base = base.trim().to_string();
    if base.is_empty() {
        bail!("base branch empty");
    }

    if !git_ok(cwd, &["rev-parse", "--abbrev-ref", "@{upstream}"]) {
        eprintln!("scrutiny forge: no upstream — git push -u origin HEAD…");
        let push = Command::new("git")
            .args(["push", "-u", "origin", "HEAD"])
            .current_dir(cwd)
            .output()
            .context("git push -u origin HEAD")?;
        if !push.status.success() {
            bail!(
                "git push failed: {}",
                String::from_utf8_lossy(&push.stderr).trim()
            );
        }
    }

    let body_path = session_root.join("pr-body.md");
    fs::write(&body_path, meta.pr_body.as_bytes())
        .with_context(|| format!("write {}", body_path.display()))?;

    let pr = Command::new("gh")
        .args([
            "pr",
            "create",
            "--draft",
            "--base",
            &base,
            "--title",
            meta.pr_title.trim(),
            "--body-file",
        ])
        .arg(&body_path)
        .current_dir(cwd)
        .output()
        .context("gh pr create")?;
    if !pr.status.success() {
        bail!(
            "gh pr create failed: {} {}",
            String::from_utf8_lossy(&pr.stderr).trim(),
            String::from_utf8_lossy(&pr.stdout).trim()
        );
    }
    let url = String::from_utf8_lossy(&pr.stdout).trim().to_string();
    eprintln!("scrutiny forge: draft PR → {url}");
    Ok(())
}

fn extract_markdownish(stdout: &str) -> String {
    // Prefer fenced markdown block
    if let Some(start) = stdout.find("```") {
        let after = &stdout[start + 3..];
        let after = after
            .strip_prefix("markdown")
            .or_else(|| after.strip_prefix("md"))
            .unwrap_or(after);
        let after = after.trim_start_matches('\n');
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }
    // JSON wrapper with text field (claude -p json)
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) {
        if let Some(t) = v.pointer("/result").and_then(|x| x.as_str()) {
            return t.to_string();
        }
        if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
            return t.to_string();
        }
    }
    stdout.trim().to_string()
}
