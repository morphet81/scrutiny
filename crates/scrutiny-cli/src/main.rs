use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use scrutiny_core::{
    load_plan_answers, partition_pack_paths, prepare_artifacts, run_agent_prompt, run_eval,
    run_findings_init, run_findings_resolve, run_findings_triage, run_findings_validate, run_forge,
    run_forge_brief, run_forge_bulk, run_forge_bulk_item, run_forge_context, run_forge_fetch,
    run_forge_plan_write, run_map, run_pack, run_parley, run_parley_fetch, run_parley_plan_write,
    run_parley_reply, run_plan_confirm, run_plan_write, run_post_comments, run_pr, run_review,
    run_review_session_write, run_scan, run_skills_install, AgentPromptInput, EvalInput,
    FindingsInitInput, ForgeBulkInput, ForgeCmdInput,
    ForgeFetchInput, ForgePlanWriteInput, ParleyAnswers, ParleyCmdInput, ParleyFetchInput,
    ParleyPlanWriteInput, ParleyReplyInput, PlanConfirmInput, PlanWriteInput, PostCommentsInput,
    PrCmdInput, ReviewCmdInput, ReviewSessionWriteInput, SkillsInstallInput,
};
use scrutiny_core::{ensure_config, find_shipped_default, load_config};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "scrutiny",
    version,
    about = "PR complexity eval + forge ticket implement helpers",
    help_template = "\
{about-with-newline}
Main commands:
  probe   Orchestrate full probe: analyze → plan → headless agents → triage → post
  forge   Orchestrate ticket implement: fetch → knobs → optional TDD plan → agent
  parley  Address unresolved PR review comments: fetch → fix agents → commit/push → reply

{usage-heading} {usage}

{all-args}{after-help}"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Evaluate change complexity vs base branch; write eval JSON; print path
    Eval {
        /// Working directory (git repo). Default: cwd
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Override base ref (PR mode)
        #[arg(long)]
        base: Option<String>,
        /// Override head ref (default HEAD)
        #[arg(long)]
        head: Option<String>,
        /// AI client key for suggested plan (cursor|claude|codex)
        #[arg(long)]
        client: Option<String>,
        /// PR number/URL → artifacts under `.scrutiny/<pr>/` (else `local` or infer)
        #[arg(long)]
        pr: Option<String>,
    },
    /// Build change map from an eval JSON; write map JSON; print path
    Map {
        /// Path to eval JSON from `scrutiny eval`
        #[arg(long)]
        eval: PathBuf,
        /// Working directory (git repo). Default: cwd
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Pack diffs + symbol slices + doc digests for AI; write pack JSON; print path
    Pack {
        #[arg(long)]
        map: PathBuf,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Deterministic scan → caveman-shaped findings JSON; print path
    Scan {
        #[arg(long)]
        map: PathBuf,
        #[arg(long)]
        pack: Option<PathBuf>,
        #[arg(long)]
        eval: Option<PathBuf>,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Interactively confirm plan knobs (all in one stdin session); print answers JSON path
    PlanConfirm {
        #[arg(long)]
        eval: PathBuf,
        #[arg(long)]
        client: Option<String>,
        #[arg(long)]
        spawn_mode: Option<String>,
        /// Skip stdin; pass PlanAnswers JSON
        #[arg(long)]
        from_json: Option<String>,
    },
    /// Write confirmed plan.json (includes skip_ai); print path
    PlanWrite {
        #[arg(long)]
        eval: PathBuf,
        #[arg(long)]
        map: Option<PathBuf>,
        #[arg(long)]
        pack: Option<PathBuf>,
        #[arg(long)]
        scan: Option<PathBuf>,
        #[arg(long)]
        client: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, action = clap::ArgAction::Set, value_parser = parse_bool_arg)]
        security: Option<bool>,
        #[arg(long, action = clap::ArgAction::Set, value_parser = parse_bool_arg)]
        performance: Option<bool>,
        #[arg(long, action = clap::ArgAction::Set, value_parser = parse_bool_arg)]
        error_handling: Option<bool>,
        #[arg(long)]
        reviewers: Option<u32>,
        #[arg(long)]
        evangelists: Option<u32>,
        #[arg(long)]
        spawn_mode: Option<String>,
        /// Path to plan-answers JSON from plan-confirm
        #[arg(long)]
        answers: Option<PathBuf>,
        /// Alternate: pass a JSON object with plan-write fields (or PlanAnswers)
        #[arg(long)]
        from_json: Option<String>,
    },
    /// Orchestrate full probe: analyze → plan → headless agents → triage → post
    #[command(hide = true)]
    Probe {
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// PR URL or number (else local branch)
        #[arg(long)]
        pr: Option<String>,
        /// Positional alias for --pr
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
        #[arg(long)]
        client: Option<String>,
        /// team (default) | isolated
        #[arg(long)]
        spawn_mode: Option<String>,
        #[arg(long)]
        from_json: Option<String>,
        #[arg(long, default_value_t = false)]
        skip_agents: bool,
        #[arg(long)]
        event: Option<String>,
        /// Skip interactive client/spawn/triage prompts
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Resume from AI review-report.json (skip eval/map/pack/scan/agents)
        #[arg(long, alias = "form-report")]
        from_report: Option<PathBuf>,
        /// Optional scan JSON when using --from-report (else empty findings shell)
        #[arg(long)]
        scan: Option<PathBuf>,
    },
    /// Install scrutiny skills via `npx skills add` (global or project)
    SkillsInstall {
        #[arg(long, short = 'g', default_value_t = false)]
        global: bool,
        #[arg(long, default_value = "*")]
        skill: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, short = 'y', default_value_t = false)]
        yes: bool,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Partition pack paths across N reviewers; print JSON array of path arrays
    PackPartition {
        #[arg(long)]
        pack: PathBuf,
        #[arg(long)]
        reviewers: u32,
    },
    /// Print agent prompt text (isolated role or team lead) for paste/debug
    AgentPrompt {
        /// reviewer|evangelist|security|performance|error_handling|lead
        #[arg(long)]
        role: String,
        #[arg(long)]
        pack: PathBuf,
        /// Confirmed plan JSON (flags/counts). Optional — defaults analyses on.
        #[arg(long)]
        plan: Option<PathBuf>,
        /// Comma-separated paths for isolated role
        #[arg(long)]
        paths: Option<String>,
    },
    /// Record spawned probe agents; validate counts vs plan; print session path
    ProbeSessionWrite {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        pack: Option<PathBuf>,
        /// JSON array of {role,index,paths,findings_count} or {agents:[…]}
        #[arg(long)]
        from_json: String,
    },
    /// Interactive Post/Ignore triage for findings JSON; print path
    FindingsTriage {
        #[arg(long)]
        findings: PathBuf,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Address unresolved PR review comments: fetch → fix agents → commit/push → reply
    #[command(hide = true)]
    Parley {
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// PR URL or number (else current branch PR)
        #[arg(long)]
        pr: Option<String>,
        /// Positional alias for --pr
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
        #[arg(long)]
        client: Option<String>,
        /// isolated (default) | team
        #[arg(long)]
        spawn_mode: Option<String>,
        /// Skip menus; pass ParleyAnswers JSON
        #[arg(long)]
        from_json: Option<String>,
        /// Non-interactive defaults
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Fetch + plan only (no headless agents)
        #[arg(long, default_value_t = false)]
        skip_agents: bool,
        /// Skip commit/push/reply
        #[arg(long, default_value_t = false)]
        skip_ship: bool,
    },
    /// Create a PR: suggest title + description from ticket → confirm/edit → gh pr create
    Pr {
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Ticket URL / key / number (else infer from branch, else prompt)
        #[arg(long)]
        ticket: Option<String>,
        /// Positional alias for --ticket
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
        /// Force ticket source (jira|github|gitlab|inline)
        #[arg(long)]
        source: Option<String>,
        /// Create a ready PR instead of the default draft
        #[arg(long, default_value_t = false)]
        ready: bool,
        /// Non-interactive: accept suggestions, skip prompts
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Fetch unresolved PR review threads → parley-comments.json path
    ParleyFetch {
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        pr: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
    /// Write parley-plan.json from comments + answers
    ParleyPlanWrite {
        #[arg(long)]
        comments: PathBuf,
        #[arg(long)]
        from_json: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Post thread replies from parley-fixes.json
    ParleyReply {
        #[arg(long)]
        fixes: PathBuf,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Orchestrate ticket implement: fetch → knobs → optional TDD plan → agent.
    ///
    /// `scrutiny forge bulk` runs several tickets at once — each on its own
    /// branch + worktree, concurrently, with the commit/PR conclude serialized
    /// on this terminal. Bulk flags (after `bulk`): `--dry` (no agents, no PR,
    /// offers to delete the branches/worktrees at the end), `--concurrency N`
    /// (cap, default `forge.bulk_concurrency`), `--yes` (headless: keys from
    /// stdin, auto commit + draft PR).
    #[command(hide = true)]
    Forge {
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// URL, issue key/number, or description
        #[arg(long)]
        input: Option<String>,
        /// Positional alias for --input
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
        #[arg(long, default_value_t = false)]
        inline: bool,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        client: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Skip menus; pass ForgeAnswers JSON
        #[arg(long)]
        from_json: Option<String>,
        /// Non-interactive defaults (no TTY menus)
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Internal: run one `forge bulk` item as a child driver process.
    #[command(hide = true)]
    ForgeBulkItem {
        /// Path to the item plan JSON written by the orchestrator.
        #[arg(long)]
        item: PathBuf,
        /// Headless (captured child, no panes, auto commit + draft PR).
        #[arg(long, default_value_t = false)]
        headless: bool,
        /// Dry run: spawn no agents, guess pr.json, no real PR.
        #[arg(long, default_value_t = false)]
        dry: bool,
    },
    /// Fetch ticket (jira|github|gitlab|inline) → ticket JSON path
    ForgeFetch {
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// URL, issue key/number, or description text
        #[arg(long)]
        input: Option<String>,
        /// Positional alias for --input
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
        /// Force source: jira|github|gitlab|inline
        #[arg(long)]
        source: Option<String>,
        /// Treat input as inline description
        #[arg(long, default_value_t = false)]
        inline: bool,
        #[arg(long)]
        client: Option<String>,
        #[arg(long)]
        title: Option<String>,
    },
    /// Write confirmed forge session plan JSON; print path
    ForgePlanWrite {
        #[arg(long)]
        ticket: PathBuf,
        #[arg(long)]
        client: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        approach: String,
        #[arg(long, action = clap::ArgAction::Set, value_parser = parse_bool_arg)]
        e2e: bool,
        #[arg(long)]
        agents: u32,
        #[arg(long)]
        testers: u32,
        #[arg(long)]
        reviewers: u32,
        #[arg(long)]
        evangelists: u32,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        from_json: Option<String>,
    },
    /// Keyword/context pack for forge agents; print path
    ForgeContext {
        #[arg(long)]
        ticket: PathBuf,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Caveman brief markdown (+ JSON) for subagent prompts; print JSON path
    ForgeBrief {
        #[arg(long)]
        ticket: PathBuf,
        #[arg(long)]
        session: Option<PathBuf>,
        #[arg(long)]
        context: Option<PathBuf>,
    },
    /// Seed findings triage JSON from scan; print path
    FindingsInit {
        #[arg(long)]
        scan: PathBuf,
        #[arg(long)]
        eval: Option<PathBuf>,
        #[arg(long)]
        pack: Option<PathBuf>,
        #[arg(long)]
        plan: Option<PathBuf>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// PR number or URL (else gh pr view for current branch)
        #[arg(long)]
        pr: Option<String>,
    },
    /// Verify/resolve line anchors against head blob; rewrite findings JSON
    FindingsResolve {
        #[arg(long)]
        findings: PathBuf,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        strict: bool,
    },
    /// Validate triage completeness before post
    FindingsValidate {
        #[arg(long)]
        findings: PathBuf,
    },
    /// Post included findings as a GitHub PR review (prompts for COMMENT/REQUEST_CHANGES/APPROVE)
    PostComments {
        #[arg(long)]
        findings: PathBuf,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        strict: bool,
        /// Skip interactive prompt (COMMENT|REQUEST_CHANGES|APPROVE)
        #[arg(long)]
        event: Option<String>,
    },
}

fn parse_bool_arg(s: &str) -> Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "y" | "on" => Ok(true),
        "false" | "0" | "no" | "n" | "off" => Ok(false),
        other => Err(format!("expected true/false, got {other}")),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("scrutiny error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Commands::Eval {
            cwd,
            base,
            head,
            client,
            pr,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            prepare_artifacts(&cwd, pr.as_deref(), &[])?;
            let (_report, path) = run_eval(EvalInput {
                cwd,
                head,
                base,
                client,
            })?;
            println!("{}", path.display());
        }
        Commands::Map { eval, cwd } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            prepare_artifacts(&cwd, None, &[&eval])?;
            let (_report, path) = run_map(&eval, &cwd)?;
            println!("{}", path.display());
        }
        Commands::Pack { map, cwd } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            prepare_artifacts(&cwd, None, &[&map])?;
            let (_report, path) = run_pack(&map, &cwd)?;
            println!("{}", path.display());
        }
        Commands::Scan {
            map,
            pack,
            eval,
            cwd,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            let mut hints: Vec<&std::path::Path> = vec![map.as_path()];
            if let Some(p) = &pack {
                hints.push(p.as_path());
            }
            if let Some(p) = &eval {
                hints.push(p.as_path());
            }
            prepare_artifacts(&cwd, None, &hints)?;
            let (_report, path) = run_scan(&map, pack.as_deref(), eval.as_deref(), &cwd)?;
            println!("{}", path.display());
        }
        Commands::PlanConfirm {
            eval,
            client,
            spawn_mode,
            from_json,
        } => {
            let cwd = std::env::current_dir().expect("cwd");
            prepare_artifacts(&cwd, None, &[eval.as_path()])?;
            let (_answers, path) = run_plan_confirm(PlanConfirmInput {
                eval_path: eval,
                client,
                spawn_mode,
                from_json,
            })?;
            println!("{}", path.display());
        }
        Commands::PlanWrite {
            eval,
            map,
            pack,
            scan,
            client,
            model,
            security,
            performance,
            error_handling,
            reviewers,
            evangelists,
            spawn_mode,
            answers,
            from_json,
        } => {
            let cwd = std::env::current_dir().expect("cwd");
            let mut hints = vec![eval.as_path()];
            if let Some(p) = &map {
                hints.push(p.as_path());
            }
            if let Some(p) = &pack {
                hints.push(p.as_path());
            }
            if let Some(p) = &scan {
                hints.push(p.as_path());
            }
            if let Some(p) = &answers {
                hints.push(p.as_path());
            }
            prepare_artifacts(&cwd, None, &hints)?;
            let mut input = resolve_plan_write_input(
                eval,
                map,
                pack,
                scan,
                client,
                model,
                security,
                performance,
                error_handling,
                reviewers,
                evangelists,
                answers,
                from_json,
            )?;
            if let Some(m) = spawn_mode {
                input.spawn_mode = m;
            }
            let (_plan, path) = run_plan_write(input)?;
            println!("{}", path.display());
        }
        Commands::Probe {
            cwd,
            pr,
            rest,
            client,
            spawn_mode,
            from_json,
            skip_agents,
            event,
            yes,
            from_report,
            scan,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            let pr = pr.or_else(|| {
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.join(" "))
                }
            });
            let (findings, _report) = run_review(ReviewCmdInput {
                cwd,
                pr,
                client,
                spawn_mode,
                from_json,
                skip_agents,
                event,
                non_interactive: yes,
                from_report,
                scan_path: scan,
            })?;
            println!("{}", findings.display());
        }
        Commands::SkillsInstall {
            global,
            skill,
            agent,
            yes,
            source,
            cwd,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            prepare_artifacts(&cwd, None, &[])?;
            run_skills_install(SkillsInstallInput {
                cwd,
                global,
                skill,
                agent,
                yes,
                source,
            })?;
        }
        Commands::PackPartition { pack, reviewers } => {
            let cwd = std::env::current_dir().expect("cwd");
            prepare_artifacts(&cwd, None, &[pack.as_path()])?;
            let buckets = partition_pack_paths(&pack, reviewers)?;
            println!("{}", serde_json::to_string(&buckets)?);
        }
        Commands::AgentPrompt {
            role,
            pack,
            plan,
            paths,
        } => {
            let cwd = std::env::current_dir().expect("cwd");
            let mut hints = vec![pack.as_path()];
            if let Some(p) = &plan {
                hints.push(p.as_path());
            }
            prepare_artifacts(&cwd, None, &hints)?;
            let paths: Vec<String> = paths
                .map(|s| {
                    s.split(',')
                        .map(|p| p.trim().to_string())
                        .filter(|p| !p.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let text = run_agent_prompt(AgentPromptInput {
                role,
                pack_path: pack,
                plan_path: plan,
                paths,
            })?;
            println!("{text}");
        }
        Commands::ProbeSessionWrite {
            plan,
            pack,
            from_json,
        } => {
            let cwd = std::env::current_dir().expect("cwd");
            let mut hints = vec![plan.as_path()];
            if let Some(p) = &pack {
                hints.push(p.as_path());
            }
            prepare_artifacts(&cwd, None, &hints)?;
            let (_session, path) = run_review_session_write(ReviewSessionWriteInput {
                plan_path: plan,
                pack_path: pack,
                from_json,
            })?;
            println!("{}", path.display());
        }
        Commands::FindingsTriage { findings, cwd } => {
            let cwd = cwd.or_else(|| std::env::current_dir().ok());
            if let Some(ref c) = cwd {
                prepare_artifacts(c, None, &[findings.as_path()])?;
            }
            let (_r, path) = run_findings_triage(&findings, cwd.as_deref(), None)?;
            println!("{}", path.display());
        }
        Commands::Parley {
            cwd,
            pr,
            rest,
            client,
            spawn_mode,
            from_json,
            yes,
            skip_agents,
            skip_ship,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            let pr = pr.or_else(|| {
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.join(" "))
                }
            });
            let path = run_parley(ParleyCmdInput {
                cwd,
                pr,
                client,
                spawn_mode,
                from_json,
                non_interactive: yes,
                skip_agents,
                skip_ship,
            })?;
            println!("{}", path.display());
        }
        Commands::Pr {
            cwd,
            ticket,
            rest,
            source,
            ready,
            yes,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            let ticket = ticket.or_else(|| {
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.join(" "))
                }
            });
            let path = run_pr(PrCmdInput {
                cwd,
                ticket,
                source,
                ready,
                non_interactive: yes,
            })?;
            println!("{}", path.display());
        }
        Commands::ParleyFetch { cwd, pr, rest } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            let pr = pr.or_else(|| {
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.join(" "))
                }
            });
            let (_file, path) = run_parley_fetch(ParleyFetchInput { cwd, pr })?;
            println!("{}", path.display());
        }
        Commands::ParleyPlanWrite {
            comments,
            from_json,
            cwd,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            prepare_artifacts(&cwd, None, &[comments.as_path()])?;
            let shipped = find_shipped_default(
                &std::env::current_exe().unwrap_or_else(|_| cwd.clone()),
            );
            let cfg_path = ensure_config(&shipped)?;
            let cfg = load_config(&cfg_path)?;
            let answers: ParleyAnswers =
                serde_json::from_str(&from_json).context("parse --from-json for parley-plan-write")?;
            let text = std::fs::read_to_string(&comments)
                .with_context(|| format!("read {}", comments.display()))?;
            let comments_file: scrutiny_core::parley::ParleyCommentsFile =
                serde_json::from_str(&text).context("parse parley-comments")?;
            let (_plan, path) = run_parley_plan_write(ParleyPlanWriteInput {
                comments: comments_file,
                comments_path: comments,
                answers,
                cfg,
            })?;
            println!("{}", path.display());
        }
        Commands::ParleyReply { fixes, cwd } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            prepare_artifacts(&cwd, None, &[fixes.as_path()])?;
            let (result, path) = run_parley_reply(ParleyReplyInput {
                fixes_path: fixes,
                cwd,
            })?;
            eprintln!(
                "scrutiny parley-reply: posted {} reply(ies) ({} skipped)",
                result.posted, result.skipped
            );
            println!("{}", path.display());
        }
        Commands::Forge {
            cwd,
            input,
            rest,
            inline,
            source,
            client,
            title,
            from_json,
            yes,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            // `scrutiny forge bulk [--dry] [--concurrency N]` → bulk orchestrator.
            if input.is_none() && rest.first().map(String::as_str) == Some("bulk") {
                // Flags after `bulk` land in `rest` (trailing_var_arg), not their
                // own clap fields — parse them here.
                let dry = rest.iter().any(|t| t == "--dry");
                let non_interactive = yes || rest.iter().any(|t| t == "--yes");
                let concurrency = rest
                    .iter()
                    .position(|t| t == "--concurrency")
                    .and_then(|i| rest.get(i + 1))
                    .and_then(|v| v.parse::<usize>().ok());
                let sessions = run_forge_bulk(ForgeBulkInput {
                    cwd,
                    client,
                    source,
                    non_interactive,
                    concurrency,
                    dry,
                })?;
                for p in sessions {
                    println!("{}", p.display());
                }
                return Ok(());
            }
            let input = input.or_else(|| {
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.join(" "))
                }
            });
            let path = run_forge(ForgeCmdInput {
                cwd,
                input,
                inline,
                source,
                client,
                title,
                from_json,
                non_interactive: yes,
            })?;
            println!("{}", path.display());
        }
        Commands::ForgeBulkItem {
            item,
            headless,
            dry,
        } => {
            run_forge_bulk_item(&item, headless, dry)?;
        }
        Commands::ForgeFetch {
            cwd,
            input,
            rest,
            source,
            inline,
            client,
            title,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            prepare_artifacts(&cwd, None, &[])?;
            let input = input.or_else(|| {
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.join(" "))
                }
            });
            let (_report, path) = run_forge_fetch(ForgeFetchInput {
                cwd,
                input,
                source,
                inline,
                client,
                title,
            })?;
            println!("{}", path.display());
        }
        Commands::ForgePlanWrite {
            ticket,
            client,
            model,
            approach,
            e2e,
            agents,
            testers,
            reviewers,
            evangelists,
            cwd,
            from_json,
        } => {
            let cwd_path = cwd
                .clone()
                .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            prepare_artifacts(&cwd_path, None, &[ticket.as_path()])?;
            let input = if let Some(raw) = from_json {
                let mut v: ForgePlanWriteInput =
                    serde_json::from_str(&raw).context("parse --from-json for forge-plan-write")?;
                if v.ticket_path.as_os_str().is_empty() {
                    v.ticket_path = ticket;
                }
                v
            } else {
                ForgePlanWriteInput {
                    ticket_path: ticket,
                    client,
                    model,
                    approach: approach.clone(),
                    e2e,
                    agents,
                    testers,
                    reviewers,
                    evangelists,
                    cwd,
                    spawn_mode: "single".into(),
                    use_playwright: false,
                    coverage_pct: 100,
                    tdd: approach.eq_ignore_ascii_case("tdd"),
                    tdd_plan_path: None,
                    figma_dir: None,
                }
            };
            let (_plan, path) = run_forge_plan_write(input)?;
            println!("{}", path.display());
        }
        Commands::ForgeContext { ticket, cwd } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            prepare_artifacts(&cwd, None, &[ticket.as_path()])?;
            let (_report, path) = run_forge_context(&ticket, &cwd)?;
            println!("{}", path.display());
        }
        Commands::ForgeBrief {
            ticket,
            session,
            context,
        } => {
            let cwd = std::env::current_dir().expect("cwd");
            let mut hints = vec![ticket.as_path()];
            if let Some(p) = &session {
                hints.push(p.as_path());
            }
            if let Some(p) = &context {
                hints.push(p.as_path());
            }
            prepare_artifacts(&cwd, None, &hints)?;
            let (_report, path) =
                run_forge_brief(&ticket, session.as_deref(), context.as_deref())?;
            println!("{}", path.display());
        }
        Commands::FindingsInit {
            scan,
            eval,
            pack,
            plan,
            cwd,
            pr,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            let mut hints = vec![scan.as_path()];
            if let Some(p) = &eval {
                hints.push(p.as_path());
            }
            if let Some(p) = &pack {
                hints.push(p.as_path());
            }
            if let Some(p) = &plan {
                hints.push(p.as_path());
            }
            prepare_artifacts(&cwd, pr.as_deref(), &hints)?;
            let (_report, path) = run_findings_init(FindingsInitInput {
                cwd,
                scan_path: scan,
                eval_path: eval,
                pack_path: pack,
                plan_path: plan,
                pr,
            })?;
            println!("{}", path.display());
        }
        Commands::FindingsResolve {
            findings,
            cwd,
            strict,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            prepare_artifacts(&cwd, None, &[findings.as_path()])?;
            let (_report, path) = run_findings_resolve(&findings, &cwd, strict)?;
            println!("{}", path.display());
        }
        Commands::FindingsValidate { findings } => {
            let cwd = std::env::current_dir().expect("cwd");
            prepare_artifacts(&cwd, None, &[findings.as_path()])?;
            let (_report, path) = run_findings_validate(&findings)?;
            println!("{}", path.display());
        }
        Commands::PostComments {
            findings,
            cwd,
            strict,
            event,
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            prepare_artifacts(&cwd, None, &[findings.as_path()])?;
            let (result, path) = run_post_comments(PostCommentsInput {
                findings_path: findings,
                cwd,
                strict,
                event,
            })?;
            eprintln!(
                "scrutiny post-comments: posted {} comment(s) as {} → {}",
                result.posted_comments,
                result.event,
                path.display()
            );
            if let Some(url) = &result.html_url {
                eprintln!("  {url}");
            }
            if !result.failed.is_empty() {
                eprintln!(
                    "scrutiny post-comments: {} finding(s) failed to anchor",
                    result.failed.len()
                );
            }
            println!("{}", path.display());
        }
    }
    Ok(())
}

fn resolve_plan_write_input(
    eval: PathBuf,
    map: Option<PathBuf>,
    pack: Option<PathBuf>,
    scan: Option<PathBuf>,
    client: Option<String>,
    model: Option<String>,
    security: Option<bool>,
    performance: Option<bool>,
    error_handling: Option<bool>,
    reviewers: Option<u32>,
    evangelists: Option<u32>,
    answers: Option<PathBuf>,
    from_json: Option<String>,
) -> Result<PlanWriteInput> {
    // Prefer --answers file, then --from-json as PlanAnswers, else flags / full PlanWriteInput JSON.
    if let Some(path) = answers {
        let a = load_plan_answers(&path)?;
        return Ok(PlanWriteInput {
            client: a.client,
            model: a.model,
            security: a.security,
            performance: a.performance,
            error_handling: a.error_handling,
            reviewers: a.reviewers,
            evangelists: a.evangelists,
            spawn_mode: a.spawn_mode,
            eval_path: eval,
            map_path: map,
            pack_path: pack,
            scan_path: scan,
        });
    }

    if let Some(raw) = from_json {
        let v: serde_json::Value =
            serde_json::from_str(&raw).context("parse --from-json for plan-write")?;
        // PlanAnswers from plan-confirm (no eval_path) vs full PlanWriteInput JSON
        if v.get("eval_path").is_none() {
            let a: scrutiny_core::PlanAnswers =
                serde_json::from_value(v).context("parse plan-answers --from-json")?;
            return Ok(PlanWriteInput {
                client: a.client,
                model: a.model,
                security: a.security,
                performance: a.performance,
                error_handling: a.error_handling,
                reviewers: a.reviewers,
                evangelists: a.evangelists,
                spawn_mode: a.spawn_mode,
                eval_path: eval,
                map_path: map,
                pack_path: pack,
                scan_path: scan,
            });
        }
        let mut input: PlanWriteInput =
            serde_json::from_value(v).context("parse PlanWriteInput --from-json")?;
        if input.eval_path.as_os_str().is_empty() {
            input.eval_path = eval;
        }
        if input.map_path.is_none() {
            input.map_path = map;
        }
        if input.pack_path.is_none() {
            input.pack_path = pack;
        }
        if input.scan_path.is_none() {
            input.scan_path = scan;
        }
        return Ok(input);
    }

    Ok(PlanWriteInput {
        client: client.context("plan-write requires --client (or --answers / --from-json)")?,
        model: model.context("plan-write requires --model (or --answers / --from-json)")?,
        security: security.context("plan-write requires --security (or --answers / --from-json)")?,
        performance: performance
            .context("plan-write requires --performance (or --answers / --from-json)")?,
        error_handling: error_handling
            .context("plan-write requires --error-handling (or --answers / --from-json)")?,
        reviewers: reviewers.unwrap_or(0),
        evangelists: evangelists.unwrap_or(0),
        spawn_mode: "team".into(),
        eval_path: eval,
        map_path: map,
        pack_path: pack,
        scan_path: scan,
    })
}
