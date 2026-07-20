//! End-to-end `scrutiny forge` orchestration.

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::agent_runner::{
    run_dry_placeholder_in, run_headless, run_nonheadless, run_nonheadless_in, wait_for_sentinels,
    HeadlessKind, HeadlessOutcome, AGENT_WALL_SECS,
};
use crate::config::{ensure_config, find_shipped_default, load_config, Config};
use crate::forge::brief::run_forge_brief;
use crate::forge::context::run_forge_context;
use crate::forge::fetch::{run_forge_fetch, ForgeFetchInput, TicketReport};
use crate::forge::figma::export_figma_designs;
use crate::forge::plan::{run_forge_plan_write, ForgePlanWriteInput, ForgeSessionPlan};
use crate::forge::scaffold;
use crate::forge::tools::playwright_cli_available;
use crate::forge::verify::{
    build_verify_plan, coverage_gaps, filter_playwright_cmd, measure_coverage, parse_test_failures,
    raw_tail, run_command, FailureReport, VerifyCmd, VerifyPlan,
};
use crate::git::{self, git_stdout};
use crate::paths::{prepare_artifacts, write_json_pretty};
use crate::runtime::{resolve_client, ResolveClientInput};
use crate::terminal::{resolve_terminal, ItemSurface, TerminalContext};

/// Shared case-title rules for TDD test-plan + implement agents.
const TEST_TITLE_GUIDELINES: &str = "\
Test case titles (it/test strings in the plan and in code):\n\
- Affirmative outcome: what the SUT does (not \"test that\" / \"verify that\" / \"ensure that\").\n\
- Start with a bare verb: renders…, returns…, opens…, shows…, calls…, does not…\n\
- Do NOT use \"should\" / \"should not\".\n\
- No prefixes: no TC-12, TEST-1, ticket ids, or numbered case labels.\n\
- Nested describe = SUT / area (symbol or feature), not a prefixed case id.\n\
- Prefer matching nearby it()/test() title style in context paths when present; \
  otherwise use bare-verb affirmative style above.\n";

/// Wall clock for a non-headless agent window (user may be watching — be generous).
const NONHEADLESS_WALL_SECS: u64 = AGENT_WALL_SECS * 3;

/// Where a forge agent runs: headless (captured), a shared visible window
/// (`term`), or a per-item surface (`surface`, bulk mode). `dry` spawns no agent.
#[derive(Clone, Copy)]
struct AgentTarget<'a> {
    term: Option<TerminalContext>,
    surface: Option<&'a ItemSurface>,
    dry: bool,
}

/// Run one forge agent either headless (captured, returns the outcome) or in a
/// visible window/surface (returns `None` — results are read from disk by the
/// caller). Forge agents always communicate via disk (test-plan.md, pr.json,
/// source, coverage), so the non-headless path needs no stdout.
fn run_forge_agent(
    client: &crate::runtime::DetectedClient,
    model: &str,
    cwd: &Path,
    prompt: &str,
    label: &str,
    target: AgentTarget,
    wall: Duration,
) -> Result<Option<HeadlessOutcome>> {
    let role = label.strip_prefix("forge-").unwrap_or(label);
    if target.dry {
        match target.surface {
            Some(surface) => run_dry_placeholder_in(cwd, role, surface)?,
            None => eprintln!("scrutiny forge: [dry] would run {role}"),
        }
        return Ok(None);
    }
    if let Some(surface) = target.surface {
        let sentinel = run_nonheadless_in(client, model, cwd, prompt, role, surface, true)?;
        let missing = wait_for_sentinels(&[sentinel], Duration::from_secs(NONHEADLESS_WALL_SECS));
        if !missing.is_empty() {
            eprintln!(
                "scrutiny forge: {role} pane did not signal done within {}s — using disk state",
                NONHEADLESS_WALL_SECS
            );
        }
        return Ok(None);
    }
    if let Some(ctx) = target.term {
        let sentinel = run_nonheadless(client, model, cwd, prompt, label, ctx)?;
        let missing = wait_for_sentinels(&[sentinel], Duration::from_secs(NONHEADLESS_WALL_SECS));
        if !missing.is_empty() {
            eprintln!(
                "scrutiny forge: {label} window did not signal done within {}s — using disk state",
                NONHEADLESS_WALL_SECS
            );
        }
        return Ok(None);
    }
    Ok(Some(run_headless(
        client,
        model,
        cwd,
        prompt,
        HeadlessKind::Forge,
        label,
        wall,
    )?))
}

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
pub(crate) struct ForgeAnswers {
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
    let mut cwd = input.cwd.clone();
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

    // Non-headless: open each agent in a visible window (claude + tmux/zellij/macOS).
    let term = resolve_terminal(cfg.headless, &detected.client, "forge");

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

    let skip_prompts = input.non_interactive || input.from_json.is_some();
    let scaffold = resolve_scaffold(&cfg, &ticket, &cwd, skip_prompts)?;
    let prefix = scaffold.prefix;
    cwd = scaffold.cwd;

    let outcome = run_forge_item_body(ForgeItemCtx {
        detected: &detected,
        cwd: cwd.clone(),
        session_root: session_root.clone(),
        ticket: &ticket,
        ticket_path: ticket_path.clone(),
        answers,
        cfg: &cfg,
        prefix: prefix.clone(),
        term,
        surface: None,
        tdd_interactive: true,
        dry: false,
    })?;

    run_forge_ship(
        &cwd,
        &session_root,
        &outcome.pr_meta_path,
        &cfg,
        skip_prompts,
        /* create_pr_noninteractive */ false,
        &prefix,
        &ticket,
    )?;

    eprintln!(
        "scrutiny forge: done. session={} ticket={} pr_meta={}",
        outcome.session_path.display(),
        ticket_path.display(),
        outcome.pr_meta_path.display()
    );
    Ok(outcome.session_path)
}

/// Everything for one item between params and ship: plan-write → context →
/// brief → (TDD plan) → implement → verify gate. Shared by single `run_forge`
/// and each bulk item driver. Returns the pr-meta + session paths.
pub(crate) struct ForgeItemCtx<'a> {
    pub detected: &'a crate::runtime::DetectedClient,
    pub cwd: PathBuf,
    pub session_root: PathBuf,
    pub ticket: &'a TicketReport,
    pub ticket_path: PathBuf,
    pub answers: ForgeAnswers,
    pub cfg: &'a Config,
    pub prefix: String,
    pub term: Option<TerminalContext>,
    pub surface: Option<ItemSurface>,
    /// When true and on a TTY, the TDD plan is validated interactively.
    pub tdd_interactive: bool,
    /// When true: spawn no agents, guess pr.json, skip the verify gate.
    pub dry: bool,
}

pub(crate) struct ForgeItemOutcome {
    pub pr_meta_path: PathBuf,
    pub session_path: PathBuf,
}

pub(crate) fn run_forge_item_body(ctx: ForgeItemCtx) -> Result<ForgeItemOutcome> {
    let ForgeItemCtx {
        detected,
        cwd,
        session_root,
        ticket,
        ticket_path,
        answers,
        cfg,
        prefix,
        term,
        surface,
        tdd_interactive,
        dry,
    } = ctx;

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
    let (cx, context_path) = run_forge_context(&ticket_path, &cwd)?;
    let (_brief, brief_path) =
        run_forge_brief(&ticket_path, Some(&session_path), Some(&context_path))?;

    let mut verify_plan = build_verify_plan(
        &cwd,
        &cfg.forge.verify_commands,
        &cx.test_harness,
        session.e2e,
        cfg.forge.verify_coverage,
        session.coverage_pct,
        cfg.forge.verify_max_loops,
    );
    // Cover the repo's pre-push checks in the gate (forge doesn't push, so the
    // hook is our only guard that the branch would survive a push).
    if let Some(cmd) =
        crate::prepush::resolve_prepush_command(&cwd, cfg.forge.prepush_cmd.as_deref())
    {
        verify_plan.commands.push(VerifyCmd {
            command: cmd,
            framework: None,
        });
    }

    let pr_meta_path = session_root.join("pr.json");
    let target = AgentTarget {
        term,
        surface: surface.as_ref(),
        dry,
    };

    if dry {
        if session.tdd {
            dry_role(&cwd, "tdd-plan", &target)?;
        }
        let impl_role = if session.spawn_mode == "team" {
            "po-team"
        } else {
            "implement"
        };
        dry_role(&cwd, impl_role, &target)?;
        write_dry_pr_meta(&pr_meta_path, ticket, &prefix)?;
        eprintln!(
            "scrutiny forge: [dry] guessed pr.json → {}",
            pr_meta_path.display()
        );
        return Ok(ForgeItemOutcome {
            pr_meta_path,
            session_path,
        });
    }

    if session.tdd {
        let plan_path = run_tdd_plan_loop(
            detected,
            &session.model,
            &cwd,
            &session_root,
            &ticket_path,
            &session_path,
            &brief_path,
            &context_path,
            &session,
            tdd_interactive,
            target,
        )?;
        session.tdd_plan_path = Some(plan_path.display().to_string());
        write_json_pretty(&session_path, &session)?;
    }

    eprintln!("scrutiny forge: implement ({})…", session.spawn_mode);
    run_implement_agent(
        detected,
        &session.model,
        &cwd,
        &ticket_path,
        &session_path,
        &brief_path,
        &context_path,
        &session,
        ticket,
        &pr_meta_path,
        &verify_plan,
        &prefix,
        target,
    )?;

    match run_verify_gate(
        detected,
        &session.model,
        &cwd,
        &ticket_path,
        &session_path,
        &brief_path,
        &context_path,
        &verify_plan,
        target,
    )? {
        GateOutcome::Green => {}
        GateOutcome::Red { proceed: true } => {
            eprintln!("scrutiny forge: verify gate red — committing anyway per user");
        }
        GateOutcome::Red { proceed: false } => {
            bail!("verify gate failed — see output above; not committing");
        }
    }

    // Optional: a dedicated headless agent rewrites the PR body from a custom prompt.
    if let Some(tmpl) = cfg.forge.pr_description_prompt.as_deref() {
        let tmpl = tmpl.trim();
        if !tmpl.is_empty() {
            if let Err(e) =
                generate_custom_pr_body(detected, &session.model, &cwd, ticket, tmpl, &pr_meta_path)
            {
                eprintln!("scrutiny forge: custom PR description skipped: {e:#}");
            }
        }
    }

    Ok(ForgeItemOutcome {
        pr_meta_path,
        session_path,
    })
}

/// Run a dedicated headless agent that writes the PR body from `prompt_tmpl` + the
/// diff, overwriting `pr.json`'s `pr_body`. Best-effort: on any failure the caller
/// logs and the existing body is kept.
fn generate_custom_pr_body(
    client: &crate::runtime::DetectedClient,
    model: &str,
    cwd: &Path,
    ticket: &TicketReport,
    prompt_tmpl: &str,
    pr_meta_path: &Path,
) -> Result<()> {
    let mut diff = git_stdout(cwd, &["diff", "HEAD"]).unwrap_or_default();
    if diff.trim().is_empty() {
        diff = git_stdout(cwd, &["diff"]).unwrap_or_default();
    }
    let prompt = format!(
        "{prompt_tmpl}\n\n\
         Write the pull-request description body for the change below.\n\
         Output ONLY the PR body as GitHub-flavored Markdown — no code fences around the \
         whole thing, no preamble, no commentary. Structure it with short `##` sections, \
         paragraphs separated by blank lines, `-` bullet lists, and `**bold**` for emphasis; \
         never one unbroken block of text.\n\n\
         ## Ticket\n{id} — {title}\n{desc}\n\n\
         ## Diff\n```diff\n{diff}\n```\n",
        id = ticket.id.trim(),
        title = ticket.title.trim(),
        desc = ticket.description.trim(),
    );
    eprintln!("scrutiny forge: generating custom PR description…");
    let out = run_headless(
        client,
        model,
        cwd,
        &prompt,
        HeadlessKind::Text,
        "forge-pr-description",
        Duration::from_secs(AGENT_WALL_SECS),
    )?;
    let body = extract_markdownish(&out.stdout);
    let body = body.trim();
    if body.is_empty() {
        bail!("PR description agent returned empty output");
    }
    let mut meta = load_pr_meta(pr_meta_path)?;
    meta.pr_body = body.to_string();
    write_json_pretty(pr_meta_path, &meta)?;
    eprintln!(
        "scrutiny forge: custom PR description written → {}",
        pr_meta_path.display()
    );
    Ok(())
}

/// Dry mode: open a role-named placeholder pane (non-headless) or just log it.
fn dry_role(cwd: &Path, role: &str, target: &AgentTarget) -> Result<()> {
    match target.surface {
        Some(surface) => run_dry_placeholder_in(cwd, role, surface),
        None => {
            eprintln!("scrutiny forge: [dry] would run {role}");
            Ok(())
        }
    }
}

/// Dry mode has no agent to author pr.json — synthesize it from ticket guesses.
fn write_dry_pr_meta(path: &Path, ticket: &TicketReport, prefix: &str) -> Result<()> {
    let meta = PrMeta {
        pr_title: scaffold::guess_pr_title(ticket, prefix),
        pr_body: scaffold::guess_pr_body(ticket),
        commit_subject: scaffold::guess_commit_subject(ticket, prefix),
        commit_body: String::new(),
    };
    write_json_pretty(path, &meta)
}

struct ScaffoldOutcome {
    prefix: String,
    cwd: PathBuf,
}

/// Guess+confirm the commit prefix, then optionally create a branch / worktree.
/// Returns the chosen prefix and the (possibly worktree) cwd for later phases.
fn resolve_scaffold(
    cfg: &Config,
    ticket: &TicketReport,
    cwd: &Path,
    skip_prompts: bool,
) -> Result<ScaffoldOutcome> {
    let theme = ColorfulTheme::default();
    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let interactive = tty && !skip_prompts;

    let guess = scaffold::guess_prefix(ticket);
    let prefix = if interactive {
        let choices = scaffold::prefix_choices(guess);
        let sel = Select::with_theme(&theme)
            .with_prompt("Commit / PR prefix")
            .items(&choices)
            .default(0)
            .interact()
            .context("prefix select")?;
        choices[sel].to_string()
    } else {
        guess.to_string()
    };

    let keep = |prefix: String| ScaffoldOutcome {
        prefix,
        cwd: cwd.to_path_buf(),
    };

    if !cfg.forge.enable_branch {
        return Ok(keep(prefix));
    }

    let repo = match git::discover_repo(cwd) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("scrutiny forge: not a git repo — skip branch step");
            return Ok(keep(prefix));
        }
    };
    let branch_name = scaffold::branch_name(ticket, &prefix);
    let on_base = git::is_base_branch(&repo.branch, &cfg.git.base_candidates);

    if !interactive {
        if cfg.forge.branch_headless == "never" || !on_base {
            return Ok(keep(prefix));
        }
        eprintln!("scrutiny forge: headless auto-branch {branch_name}");
        git::create_branch(&repo.root, &branch_name)?;
        return Ok(keep(prefix));
    }

    let items = [
        format!("Create branch: {branch_name}"),
        format!("Create branch + worktree: {branch_name}"),
        format!("No branch (use current: {})", repo.branch),
    ];
    let default_idx = if on_base { 0 } else { 2 };
    let sel = Select::with_theme(&theme)
        .with_prompt("Branch")
        .items(&items)
        .default(default_idx)
        .interact()
        .context("branch select")?;
    match sel {
        0 => {
            git::create_branch(&repo.root, &branch_name)?;
            eprintln!("scrutiny forge: branch {branch_name}");
            Ok(keep(prefix))
        }
        1 => {
            let dir = worktree_dir(&repo, &branch_name);
            let wt = git::create_worktree(&repo.root, &branch_name, &dir)?;
            eprintln!("scrutiny forge: worktree {} ({branch_name})", wt.display());
            Ok(ScaffoldOutcome { prefix, cwd: wt })
        }
        _ => Ok(keep(prefix)),
    }
}

pub(crate) fn worktree_dir(repo: &git::RepoContext, branch_name: &str) -> PathBuf {
    let san = branch_name.replace('/', "-");
    let base = repo
        .root
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| repo.root.clone());
    base.join(format!("{}-{}", repo.repo_slug, san))
}

pub(crate) fn prompt_forge_answers(client: &str, ticket: &TicketReport) -> Result<ForgeAnswers> {
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

    let model = if sug.prompt_model {
        let models = &sug.available_models;
        if models.len() > 1 {
            let default_idx = models
                .iter()
                .position(|m| m == &sug.model)
                .unwrap_or(0);
            let prompt_label = if sug.complexity_reason.is_empty() {
                format!("Model  [tier {}]", sug.tier)
            } else {
                format!("Model  [tier {} · {}]", sug.tier, sug.complexity_reason)
            };
            let sel = Select::with_theme(&theme)
                .with_prompt(prompt_label)
                .items(models)
                .default(default_idx)
                .interact()
                .context("model select")?;
            models[sel].clone()
        } else {
            sug.model.clone()
        }
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
    tdd_interactive: bool,
    target: AgentTarget,
) -> Result<PathBuf> {
    let plan_path = session_root.join("test-plan.md");
    let theme = ColorfulTheme::default();

    // Generate once up front; the loop only re-renders and (on Revise) re-runs.
    run_test_plan_agent(
        client, model, cwd, ticket_path, session_path, brief_path, context_path, session,
        &plan_path, None, "forge-test-plan", target,
    )?;

    loop {
        let plan_text = fs::read_to_string(&plan_path).context("read test-plan.md")?;
        eprintln!("\n{}\n", crate::mdterm::render_markdown(&plan_text));
        eprintln!(
            "Test plan file (open in your editor to edit):\n  {}\n",
            plan_path.display()
        );

        if !tdd_interactive || !std::io::stdin().is_terminal() {
            eprintln!("scrutiny forge: non-interactive — auto-confirm test plan");
            break;
        }

        let sel = Select::with_theme(&theme)
            .with_prompt("Test plan")
            .items(&[
                "Confirm",
                "Revise (AI edits per your comments)",
                "Edited already — re-read file",
            ])
            .default(0)
            .interact()
            .context("test plan menu")?;
        match sel {
            0 => break,
            2 => continue, // user edited on disk → re-read + re-render, no agent
            _ => {
                let comment: String = Input::with_theme(&theme)
                    .with_prompt("Your comments")
                    .interact_text()
                    .context("test plan comment")?;
                run_test_plan_agent(
                    client, model, cwd, ticket_path, session_path, brief_path, context_path,
                    session, &plan_path, Some(&comment), "forge-test-plan-revise", target,
                )?;
            }
        }
    }

    Ok(plan_path)
}

/// Run the test-plan agent (initial or revision) and ensure `plan_path` exists,
/// salvaging markdown from stdout when the agent didn't write the file.
#[allow(clippy::too_many_arguments)]
fn run_test_plan_agent(
    client: &crate::runtime::DetectedClient,
    model: &str,
    cwd: &Path,
    ticket_path: &Path,
    session_path: &Path,
    brief_path: &Path,
    context_path: &Path,
    session: &ForgeSessionPlan,
    plan_path: &Path,
    comment: Option<&str>,
    label: &str,
    target: AgentTarget,
) -> Result<()> {
    let prompt = build_test_plan_prompt(
        ticket_path,
        session_path,
        brief_path,
        context_path,
        session,
        plan_path,
        comment,
    );
    eprintln!(
        "scrutiny forge: {} test plan…",
        if comment.is_some() { "revising" } else { "generating" }
    );
    let out = run_forge_agent(
        client,
        model,
        cwd,
        &prompt,
        label,
        target,
        Duration::from_secs(AGENT_WALL_SECS),
    )?;
    match out {
        // Headless: salvage markdown from stdout if the agent didn't write the file.
        Some(o) => {
            if o.code != 0 && !o.timed_out && !plan_path.exists() {
                bail!(
                    "test-plan agent failed: {}",
                    o.stderr.chars().take(400).collect::<String>()
                );
            }
            if !plan_path.exists() {
                let text = extract_markdownish(&o.stdout);
                if text.trim().is_empty() {
                    bail!("test-plan agent produced no markdown");
                }
                fs::write(plan_path, text).context("write test-plan.md")?;
            }
        }
        // Non-headless: no stdout to salvage — the plan file is the only channel.
        None => {
            if !plan_path.exists() {
                bail!(
                    "test-plan agent window finished without writing {}",
                    plan_path.display()
                );
            }
        }
    }
    Ok(())
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
    p.push_str(crate::prepush::PREPUSH_OWNERSHIP);
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
    p.push_str("\n");
    p.push_str(TEST_TITLE_GUIDELINES);
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
    verify: &VerifyPlan,
    prefix: &str,
    target: AgentTarget,
) -> Result<()> {
    let prompt = build_implement_prompt(
        ticket_path,
        session_path,
        brief_path,
        context_path,
        session,
        ticket,
        pr_meta_path,
        verify,
        prefix,
    );
    let label = if session.spawn_mode == "team" {
        "forge-po-team"
    } else {
        "forge-implement"
    };
    let out = run_forge_agent(
        client,
        model,
        cwd,
        &prompt,
        label,
        target,
        Duration::from_secs(AGENT_WALL_SECS.saturating_mul(2)),
    )?;
    if let Some(o) = out {
        if o.code != 0 && !o.timed_out {
            eprintln!(
                "scrutiny forge: implement agent exit {} — check output",
                o.code
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_implement_prompt(
    ticket_path: &Path,
    session_path: &Path,
    brief_path: &Path,
    context_path: &Path,
    session: &ForgeSessionPlan,
    ticket: &TicketReport,
    pr_meta_path: &Path,
    verify: &VerifyPlan,
    prefix: &str,
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
    p.push_str(crate::prepush::PREPUSH_OWNERSHIP);
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
                "\nTDD: implement tests from the approved plan first (red), then production code (green).\n\
                 Use approved plan case titles verbatim as it()/test() strings.\n",
            );
        } else {
            p.push_str("\nTDD: write failing tests before production code.\n");
        }
        p.push_str(TEST_TITLE_GUIDELINES);
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
    p.push_str(&format!(
        "- pr_title: short PR title for this branch.\n\
         - pr_body: PR description in GitHub-flavored Markdown. Use short `##` sections \
         (e.g. Summary, Changes, Testing), paragraphs separated by blank lines, `-` bullet \
         lists, and `**bold**` for emphasis — never one unbroken block of text. Must reference \
         the ticket URL below (that URL only — do not invent or link a different ticket).\n\
         - commit_subject / commit_body: conventional commit message for one final commit \
         the host script will create. commit_subject MUST start with the prefix `{prefix}:` \
         (the host already picked this prefix).\n"
    ));
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
    if !verify.is_empty() {
        p.push_str("\n## Verification gate (host will enforce)\n");
        p.push_str(
            "After you finish, the host runs these and will NOT commit until they pass:\n",
        );
        for c in &verify.commands {
            p.push_str(&format!("- `{}`\n", c.command));
        }
        if verify.coverage.is_some() {
            p.push_str(&format!(
                "- line coverage must reach ~{}%\n",
                verify.coverage_target
            ));
        }
        p.push_str("Make your changes actually pass these locally before finishing.\n");
    }

    p.push_str(
        "\n## Do NOT ship yourself\n\
         Do NOT create or switch git branches — the host already put you on the right branch. \
         Do NOT run git commit, git push, or open a PR. The host script commits and may \
         create a draft PR after you finish.\n",
    );
    p
}

enum GateOutcome {
    Green,
    Red { proceed: bool },
}

#[allow(clippy::too_many_arguments)]
fn run_verify_gate(
    client: &crate::runtime::DetectedClient,
    model: &str,
    cwd: &Path,
    ticket_path: &Path,
    session_path: &Path,
    brief_path: &Path,
    context_path: &Path,
    plan: &VerifyPlan,
    target: AgentTarget,
) -> Result<GateOutcome> {
    if plan.is_empty() {
        eprintln!("scrutiny forge: no verify commands — skip gate");
        return Ok(GateOutcome::Green);
    }

    let mut cov_unmeasurable_warned = false;
    let max = plan.max_loops.max(1);
    for attempt in 1..=max {
        eprintln!(
            "scrutiny forge: verify gate (attempt {attempt}/{max})…"
        );
        let mut report = FailureReport::default();

        for cmd in &plan.commands {
            let effective_cmd = filter_playwright_cmd(cwd, cmd);
            if effective_cmd != cmd.command {
                eprintln!("scrutiny forge:   (filtered to changed spec files)");
            }
            eprintln!("scrutiny forge:   run `{}`", effective_cmd);
            let (code, out, err) = run_command(cwd, &effective_cmd);
            if code != 0 {
                let fails = parse_test_failures(cmd.framework.as_deref(), &out, &err);
                if fails.is_empty() && report.raw_tail.is_none() {
                    report.raw_tail = Some(raw_tail(&out, &err));
                }
                report.failed_tests.extend(fails);
            }
        }

        if let Some(probe) = &plan.coverage {
            match measure_coverage(cwd, probe) {
                Some(pct) => {
                    if pct + 0.5 < plan.coverage_target as f64 {
                        eprintln!(
                            "scrutiny forge:   coverage {pct:.1}% < target {}%",
                            plan.coverage_target
                        );
                        report.uncovered = coverage_gaps(&probe.framework, cwd);
                    }
                }
                None => {
                    if !cov_unmeasurable_warned {
                        eprintln!(
                            "scrutiny forge:   coverage unmeasurable — skipping coverage gate"
                        );
                        cov_unmeasurable_warned = true;
                    }
                }
            }
        }

        if report.is_clean() {
            eprintln!("scrutiny forge: verify gate green");
            return Ok(GateOutcome::Green);
        }

        if attempt == max {
            eprintln!(
                "scrutiny forge: verify gate still red after {max} attempt(s):\n{}",
                summarize_report(&report)
            );
            let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
            // Bulk items (surface) never block a pane on a prompt — record red.
            if !tty || target.surface.is_some() {
                return Ok(GateOutcome::Red { proceed: false });
            }
            let proceed = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Verify gate failed — commit anyway?")
                .default(false)
                .interact()
                .context("verify gate confirm")?;
            return Ok(GateOutcome::Red { proceed });
        }

        let prompt = build_verify_fix_prompt(
            ticket_path,
            session_path,
            brief_path,
            context_path,
            &report,
            plan.coverage_target,
        );
        eprintln!("scrutiny forge: spawning fix agent…");
        let out = run_forge_agent(
            client,
            model,
            cwd,
            &prompt,
            "forge-verify-fix",
            target,
            Duration::from_secs(AGENT_WALL_SECS.saturating_mul(2)),
        )?;
        if let Some(o) = out {
            if o.code != 0 && !o.timed_out {
                eprintln!("scrutiny forge: fix agent exit {} — re-checking", o.code);
            }
        }
    }

    unreachable!("loop returns on final attempt")
}

fn summarize_report(report: &FailureReport) -> String {
    let mut s = String::new();
    for f in &report.failed_tests {
        let loc = match (&f.file, f.line) {
            (Some(file), Some(line)) => format!("{file}:{line} — "),
            (Some(file), None) => format!("{file} — "),
            _ => String::new(),
        };
        s.push_str(&format!("  ✗ {loc}{}: {}\n", f.name, f.message));
    }
    for g in &report.uncovered {
        s.push_str(&format!("  ○ {} — lines {}\n", g.file, g.lines));
    }
    if let Some(tail) = &report.raw_tail {
        s.push_str(&format!("  (unparsed output)\n{tail}\n"));
    }
    s
}

fn build_verify_fix_prompt(
    ticket_path: &Path,
    session_path: &Path,
    brief_path: &Path,
    context_path: &Path,
    report: &FailureReport,
    coverage_target: u32,
) -> String {
    let mut p = String::new();
    p.push_str(
        "The previous attempt failed the verify gate. Fix ONLY what is listed below.\n\
         Do NOT weaken, skip, or delete tests. Do NOT commit, push, or open a PR.\n\
         Do NOT run the tests, lint, build, or any check command yourself — \
         scrutiny re-runs them and verifies your fix.\n",
    );
    p.push_str("\nContext (already on disk — read only if needed):\n");
    p.push_str(&format!("- ticket: {}\n", ticket_path.display()));
    p.push_str(&format!("- session: {}\n", session_path.display()));
    p.push_str(&format!("- brief: {}\n", brief_path.display()));
    p.push_str(&format!("- context: {}\n", context_path.display()));

    if !report.failed_tests.is_empty() {
        p.push_str("\n### Failing tests\n");
        for f in &report.failed_tests {
            let loc = match (&f.file, f.line) {
                (Some(file), Some(line)) => format!("`{file}:{line}` — "),
                (Some(file), None) => format!("`{file}` — "),
                _ => String::new(),
            };
            p.push_str(&format!("- {loc}{}: {}\n", f.name, f.message));
        }
    }

    if !report.uncovered.is_empty() {
        p.push_str(&format!(
            "\n### Uncovered lines (coverage target {coverage_target}%)\n"
        ));
        for g in &report.uncovered {
            p.push_str(&format!("- `{}` — lines {}\n", g.file, g.lines));
        }
    }

    if let Some(tail) = &report.raw_tail {
        p.push_str(
            "\n### Raw output (structured parsing unavailable — inspect manually)\n```\n",
        );
        p.push_str(tail);
        p.push_str("\n```\n");
    }

    p.push_str(
        "\nOpen only the files named above. Add/fix code and tests so these pass \
         and the listed lines are covered, then stop.\n",
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
    // pr_title / commit_subject may be empty — the host falls back to a guess.
    Ok(meta)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_forge_ship(
    cwd: &Path,
    session_root: &Path,
    pr_meta_path: &Path,
    cfg: &Config,
    skip_prompts: bool,
    create_pr_noninteractive: bool,
    prefix: &str,
    ticket: &TicketReport,
) -> Result<()> {
    let meta = load_pr_meta(pr_meta_path)?;
    eprintln!(
        "scrutiny forge: shipping metadata → {}",
        pr_meta_path.display()
    );

    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();

    // Commit subject: AI value if present, else guess — user may edit on a TTY.
    let subject_default = {
        let ai = meta.commit_subject.trim();
        if ai.is_empty() {
            crate::forge::scaffold::guess_commit_subject(ticket, prefix)
        } else {
            ai.to_string()
        }
    };
    let commit_subject = if skip_prompts || !tty {
        subject_default
    } else {
        Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Commit subject")
            .default(subject_default)
            .interact_text()
            .context("commit subject")?
            .trim()
            .to_string()
    };
    if commit_subject.is_empty() {
        bail!("commit subject empty");
    }

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

        let mut msg = commit_subject.clone();
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
        eprintln!("scrutiny forge: committed — {commit_subject}");
    }

    let suggested_title = {
        let ai = meta.pr_title.trim();
        if ai.is_empty() {
            scaffold::guess_pr_title(ticket, prefix)
        } else {
            ai.to_string()
        }
    };
    let suggested_body = {
        let ai = meta.pr_body.trim();
        if ai.is_empty() {
            scaffold::guess_pr_body(ticket)
        } else {
            meta.pr_body.clone()
        }
    };

    if skip_prompts || !tty {
        if create_pr_noninteractive {
            let choice = crate::pr::confirm_pr_meta(
                cfg,
                cwd,
                session_root,
                &suggested_title,
                &suggested_body,
                /* skip_prompts */ true,
            )?;
            let url = crate::pr::create_pr(
                cwd,
                session_root,
                &choice.base,
                &choice.title,
                &choice.body,
                /* draft */ true,
            )?;
            eprintln!("scrutiny forge: draft PR → {url}");
        } else {
            eprintln!(
                "scrutiny forge: skip draft PR prompt (non-interactive). \
                 pr.json ready at {}",
                pr_meta_path.display()
            );
        }
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

    let choice = crate::pr::confirm_pr_meta(
        cfg,
        cwd,
        session_root,
        &suggested_title,
        &suggested_body,
        skip_prompts,
    )?;
    let url = crate::pr::create_pr(
        cwd,
        session_root,
        &choice.base,
        &choice.title,
        &choice.body,
        /* draft */ true,
    )?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn session(e2e: bool, tdd: bool, plan: Option<&str>) -> ForgeSessionPlan {
        ForgeSessionPlan {
            version: 1,
            client: "claude".into(),
            model: "sonnet".into(),
            approach: if tdd { "tdd".into() } else { "heads_down".into() },
            e2e,
            agents: 1,
            testers: 1,
            reviewers: 0,
            evangelists: 0,
            enable_figma: false,
            enable_lore: false,
            enable_ticket_writeback: false,
            enable_po: false,
            ticket_path: "/tmp/ticket.json".into(),
            skip_ai_review: true,
            skip_ai_review_reason: None,
            spawn_mode: "single".into(),
            use_playwright: false,
            coverage_pct: 100,
            tdd,
            tdd_plan_path: plan.map(str::to_string),
            figma_dir: None,
        }
    }

    #[test]
    fn test_plan_prompt_includes_verb_first_title_rules() {
        let s = session(true, true, None);
        let p = build_test_plan_prompt(
            Path::new("/t/ticket.json"),
            Path::new("/t/session.json"),
            Path::new("/t/brief.md"),
            Path::new("/t/context.json"),
            &s,
            Path::new("/t/test-plan.md"),
            None,
        );
        assert!(p.contains("Start with a bare verb"));
        assert!(p.contains("No prefixes: no TC-12"));
        assert!(p.contains("Do NOT use \"should\""));
        assert!(p.contains("Affirmative outcome"));
        assert!(p.contains("and e2e cases"));
    }

    #[test]
    fn implement_prompt_includes_title_rules_when_tdd() {
        let s = session(false, true, Some("/t/test-plan.md"));
        let ticket = TicketReport {
            version: 1,
            source: "inline".into(),
            id: "inline-1".into(),
            url: None,
            title: "Add widget".into(),
            description: "Build it.".into(),
            labels: vec![],
            comments: vec![],
            attachments_dir: None,
            figma_urls: vec![],
            figma_dir: None,
            fields: serde_json::json!({}),
            raw_path: None,
            fetched_at: String::new(),
            suggested_forge: crate::config::SuggestedForge::default(),
        };
        let verify = VerifyPlan {
            commands: vec![],
            coverage: None,
            coverage_target: 100,
            max_loops: 1,
        };
        let p = build_implement_prompt(
            Path::new("/t/ticket.json"),
            Path::new("/t/session.json"),
            Path::new("/t/brief.md"),
            Path::new("/t/context.json"),
            &s,
            &ticket,
            Path::new("/t/pr.json"),
            &verify,
            "feat",
        );
        assert!(p.contains("Use approved plan case titles verbatim"));
        assert!(p.contains("Start with a bare verb"));
        assert!(p.contains("No prefixes: no TC-12"));
    }
}
