use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use scrutiny_core::{
    run_eval, run_findings_init, run_findings_resolve, run_findings_validate, run_forge_brief,
    run_forge_context, run_forge_fetch, run_forge_plan_write, run_map, run_pack, run_plan_write,
    run_post_comments, run_scan, EvalInput, FindingsInitInput, ForgeFetchInput, ForgePlanWriteInput,
    PlanWriteInput, PostCommentsInput,
};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "scrutiny",
    version,
    about = "PR complexity eval + forge ticket implement helpers"
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
        client: String,
        #[arg(long)]
        model: String,
        #[arg(long, action = clap::ArgAction::Set, value_parser = parse_bool_arg)]
        security: bool,
        #[arg(long, action = clap::ArgAction::Set, value_parser = parse_bool_arg)]
        performance: bool,
        #[arg(long, action = clap::ArgAction::Set, value_parser = parse_bool_arg)]
        error_handling: bool,
        #[arg(long, default_value_t = 0)]
        reviewers: u32,
        #[arg(long, default_value_t = 0)]
        evangelists: u32,
        /// Alternate: pass a JSON object with the same fields (overrides flags when set)
        #[arg(long)]
        from_json: Option<String>,
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
        } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
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
            let (_report, path) = run_map(&eval, &cwd)?;
            println!("{}", path.display());
        }
        Commands::Pack { map, cwd } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
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
            let (_report, path) = run_scan(&map, pack.as_deref(), eval.as_deref(), &cwd)?;
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
            from_json,
        } => {
            let input = if let Some(raw) = from_json {
                let mut v: PlanWriteInput =
                    serde_json::from_str(&raw).context("parse --from-json for plan-write")?;
                // Ensure eval path present; allow flags to fill gaps if omitted in JSON
                if v.eval_path.as_os_str().is_empty() {
                    v.eval_path = eval;
                }
                if v.map_path.is_none() {
                    v.map_path = map;
                }
                if v.pack_path.is_none() {
                    v.pack_path = pack;
                }
                if v.scan_path.is_none() {
                    v.scan_path = scan;
                }
                v
            } else {
                PlanWriteInput {
                    client,
                    model,
                    security,
                    performance,
                    error_handling,
                    reviewers,
                    evangelists,
                    eval_path: eval,
                    map_path: map,
                    pack_path: pack,
                    scan_path: scan,
                }
            };
            let (_plan, path) = run_plan_write(input)?;
            println!("{}", path.display());
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
                    approach,
                    e2e,
                    agents,
                    testers,
                    reviewers,
                    evangelists,
                    cwd,
                }
            };
            let (_plan, path) = run_forge_plan_write(input)?;
            println!("{}", path.display());
        }
        Commands::ForgeContext { ticket, cwd } => {
            let cwd = cwd.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            let (_report, path) = run_forge_context(&ticket, &cwd)?;
            println!("{}", path.display());
        }
        Commands::ForgeBrief {
            ticket,
            session,
            context,
        } => {
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
            let (_report, path) = run_findings_resolve(&findings, &cwd, strict)?;
            println!("{}", path.display());
        }
        Commands::FindingsValidate { findings } => {
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
            let (_result, path) = run_post_comments(PostCommentsInput {
                findings_path: findings,
                cwd,
                strict,
                event,
            })?;
            println!("{}", path.display());
        }
    }
    Ok(())
}
