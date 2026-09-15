//! Orchestrate end-to-end `scrutiny probe` (script-driven).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};

use crate::agent_runner::{
    collate_review_report, run_isolated_agents, run_pr_summary_agent, run_team_agents,
    session_records_from_report, summary_concerns_as_findings, ProbePrSummary, ReviewReport,
};
use crate::config::{ensure_config, find_shipped_default, load_config};
use crate::eval::{run_eval, EvalInput};
use crate::findings::{
    attach_pr_to_findings, merge_ai_findings, prompt_pr_if_missing, promote_summary_concerns,
    run_findings_init,
    run_findings_init_empty, run_findings_resolve, run_findings_triage, run_findings_validate,
    run_post_comments, FindingsInitInput, PostCommentsInput, TriageAskCtx,
};
use crate::git;
use crate::gh::resolve_pr_refs;
use crate::map::run_map;
use crate::pack::run_pack;
use crate::plan::{run_plan_confirm, run_plan_write, PlanConfirmInput, PlanWriteInput};
use crate::review_session::{run_review_session_write, ReviewSessionWriteInput};
use crate::runtime::{resolve_client, resolve_spawn_mode, DetectedClient, ResolveClientInput};
use crate::scan::run_scan;
use crate::terminal::{force_close_agent_panes, resolve_terminal, AgentPaneCleanupGuard};

#[derive(Debug, Clone)]
pub struct ReviewCmdInput {
    pub cwd: PathBuf,
    pub pr: Option<String>,
    pub client: Option<String>,
    pub spawn_mode: Option<String>,
    pub from_json: Option<String>,
    pub skip_agents: bool,
    pub event: Option<String>,
    /// Skip interactive prompts when possible (CI).
    pub non_interactive: bool,
    /// Resume: AI `review-report.json` path (skip eval/map/pack/scan/agents).
    pub from_report: Option<PathBuf>,
    /// Optional scan JSON when using `--from-report` (else empty findings shell).
    pub scan_path: Option<PathBuf>,
    /// Skip `finish_triage_and_post`; caller is responsible for calling `run_pending_triage`.
    pub skip_triage: bool,
}

/// All context needed to run triage+post after a deferred (skip_triage) review.
#[derive(Debug, Clone)]
pub struct PendingTriage {
    pub findings_path: PathBuf,
    pub cwd: PathBuf,
    pub client: Option<DetectedClient>,
    pub model: String,
    pub event: Option<String>,
    pub non_interactive: bool,
    pub pack_path: PathBuf,
    pub client_override: Option<String>,
}

pub struct ReviewResult {
    pub findings_path: PathBuf,
    pub report_path: Option<PathBuf>,
    pub answers_json: Option<String>,
    /// Populated when `skip_triage = true`; call `run_pending_triage` to complete.
    pub pending_triage: Option<PendingTriage>,
}

/// Run the triage+post phase deferred from a `skip_triage` review.
pub fn run_pending_triage(t: PendingTriage) -> Result<()> {
    finish_triage_and_post(
        &t.findings_path,
        &t.cwd,
        t.client.as_ref(),
        &t.model,
        t.event,
        t.non_interactive,
        &t.pack_path,
        t.client_override,
    )
}

#[derive(Debug, Clone)]
pub struct ReportResumeInput {
    pub report_path: PathBuf,
    pub cwd: PathBuf,
    pub pr: Option<String>,
    pub event: Option<String>,
    pub non_interactive: bool,
    pub scan_path: Option<PathBuf>,
    pub client: Option<String>,
}

pub fn run_review(input: ReviewCmdInput) -> Result<ReviewResult> {
    let cwd = input.cwd.clone();
    let hints: Vec<&Path> = input
        .from_report
        .as_deref()
        .into_iter()
        .chain(input.scan_path.as_deref())
        .collect();

    let pr_refs = resolve_pr_refs(&cwd, input.pr.as_deref())?;
    let effective_pr = input
        .pr
        .as_deref()
        .or(pr_refs.number.as_deref());
    if input.pr.is_none() {
        if let Some(n) = &pr_refs.number {
            eprintln!("scrutiny probe: using PR #{n} for current branch");
        } else {
            eprintln!("scrutiny probe: no open PR for current branch — local diff vs base");
        }
    }
    crate::paths::prepare_artifacts(&cwd, effective_pr, &hints)?;

    if let Some(report_path) = input.from_report.clone() {
        let (fp, rp) = run_review_from_report(ReportResumeInput {
            report_path,
            cwd: input.cwd,
            pr: input.pr,
            event: input.event,
            non_interactive: input.non_interactive,
            scan_path: input.scan_path,
            client: input.client,
        })?;
        return Ok(ReviewResult {
            findings_path: fp,
            report_path: rp,
            answers_json: None,
            pending_triage: None,
        });
    }

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

    let spawn_mode = resolve_spawn_mode(
        &cfg,
        input.spawn_mode.as_deref(),
        input.non_interactive || input.from_json.is_some(),
    )?;

    let pr_for_init = pr_refs.number.clone();

    // PR mode: fetch the PR's real base + head from its own repo and diff those
    // exact commits, so the review is scoped to the PR regardless of local
    // branch state or where scrutiny is invoked from.
    let (base, head) = match (&pr_refs.base, &pr_refs.number, &pr_refs.repo_url) {
        (Some(base_branch), Some(number), Some(repo_url)) => {
            eprintln!("scrutiny probe: fetch PR refs ({base_branch}…#{number})…");
            let (base_oid, head_oid) =
                git::fetch_pr_diff_refs(&cwd, repo_url, base_branch, number)?;
            (Some(base_oid), Some(head_oid))
        }
        _ => (pr_refs.base.clone(), pr_refs.head.clone()),
    };

    eprintln!("scrutiny probe: eval…");
    let (eval, eval_path) = run_eval(EvalInput {
        cwd: cwd.clone(),
        head: head.clone(),
        base: base.clone(),
        client: Some(detected.client.clone()),
    })?;
    eprintln!("  {}", eval_path.display());

    eprintln!("scrutiny probe: map…");
    let (_map, map_path) = run_map(&eval_path, &cwd)?;
    eprintln!("  {}", map_path.display());

    eprintln!("scrutiny probe: pack…");
    let (_pack, pack_path) = run_pack(&map_path, &cwd)?;
    eprintln!("  {}", pack_path.display());

    eprintln!("scrutiny probe: scan…");
    let (_scan, scan_path) = run_scan(&map_path, Some(&pack_path), Some(&eval_path), &cwd)?;
    eprintln!("  {}", scan_path.display());

    eprintln!("scrutiny probe: plan-confirm…");
    let (answers, answers_path) = run_plan_confirm(PlanConfirmInput {
        eval_path: eval_path.clone(),
        client: Some(detected.client.clone()),
        spawn_mode: Some(spawn_mode.clone()),
        from_json: input.from_json.clone(),
        accept_suggested: input.non_interactive,
    })?;
    eprintln!("  {}", answers_path.display());
    let answers_json = serde_json::to_string(&answers).ok();

    let (plan, plan_path) = run_plan_write(PlanWriteInput {
        client: answers.client.clone(),
        model: answers.model.clone(),
        security: answers.security,
        performance: answers.performance,
        error_handling: answers.error_handling,
        reviewers: answers.reviewers,
        evangelists: answers.evangelists,
        spawn_mode: answers.spawn_mode.clone(),
        eval_path: eval_path.clone(),
        map_path: Some(map_path.clone()),
        pack_path: Some(pack_path.clone()),
        scan_path: Some(scan_path.clone()),
    })?;
    eprintln!("scrutiny probe: plan {}", plan_path.display());

    let mut report_path: Option<PathBuf> = None;

    if !plan.skip_ai && !input.skip_agents {
        let term = resolve_terminal(cfg.headless, &detected.client, "probe");
        // Force-close leftover agent panes on exit / Ctrl-C / unwind (after summary joins).
        let _pane_guard = term.as_ref().map(|_| AgentPaneCleanupGuard::default());

        let summary_handle = spawn_pr_summary_agent(
            cfg.probe.pr_summary,
            &detected,
            &plan.model,
            &pack_path,
            &cwd,
            term.clone(),
        );

        let spawn_mode = plan.spawn_mode.as_str();
        let agents = if spawn_mode == "team" {
            eprintln!("scrutiny probe: team lead agent…");
            run_team_agents(&detected, &plan, &pack_path, &cwd, term.as_ref())?
        } else {
            eprintln!("scrutiny probe: isolated parallel agents…");
            run_isolated_agents(&detected, &plan, &pack_path, &cwd, term.as_ref())?
        };

        let pr_summary = join_pr_summary_agent(summary_handle);
        // Summary may have shared the pane pool; close leftovers before consolidate/triage.
        force_close_agent_panes();

        let extra = pr_summary
            .as_ref()
            .map(summary_concerns_as_findings)
            .unwrap_or_default();
        if !extra.is_empty() {
            eprintln!(
                "scrutiny probe: folding {} summary concern(s) into consolidator",
                extra.len()
            );
        }

        let (report, rpath) = collate_review_report(
            agents,
            spawn_mode,
            &detected,
            &plan.model,
            &pack_path,
            &cwd,
            extra,
        )?;
        eprintln!(
            "  report {} ({} findings, from {} raw)",
            rpath.display(),
            report.findings.len(),
            report.deduped_from
        );
        report_path = Some(rpath);

        let agents_json = serde_json::to_string(&session_records_from_report(&report))?;
        match run_review_session_write(ReviewSessionWriteInput {
            plan_path: plan_path.clone(),
            pack_path: Some(pack_path.clone()),
            from_json: agents_json,
        }) {
            Ok((_, sp)) => eprintln!("  session {}", sp.display()),
            Err(e) => eprintln!("scrutiny probe: warn: probe-session-write: {e:#}"),
        }

        eprintln!("scrutiny probe: findings-init…");
        let (_fr, findings_path) = run_findings_init(FindingsInitInput {
            cwd: cwd.clone(),
            scan_path: scan_path.clone(),
            eval_path: Some(eval_path.clone()),
            pack_path: Some(pack_path.clone()),
            plan_path: Some(plan_path.clone()),
            pr: pr_for_init.clone(),
            pr_summary,
        })?;
        merge_ai_findings(&findings_path, &report.findings)?;
        promote_summary_concerns(&findings_path)?;
        eprintln!(
            "scrutiny probe: merged {} AI findings → {}",
            report.findings.len(),
            findings_path.display()
        );
        if input.skip_triage {
            let pending = PendingTriage {
                findings_path: findings_path.clone(),
                cwd: cwd.clone(),
                client: Some(detected.clone()),
                model: plan.model.clone(),
                event: input.event.clone(),
                non_interactive: input.non_interactive,
                pack_path: pack_path.clone(),
                client_override: None,
            };
            return Ok(ReviewResult {
                findings_path,
                report_path,
                answers_json,
                pending_triage: Some(pending),
            });
        }
        finish_triage_and_post(
            &findings_path,
            &cwd,
            Some(&detected),
            &plan.model,
            input.event.clone(),
            input.non_interactive,
            &pack_path,
            None,
        )?;
        return Ok(ReviewResult {
            findings_path,
            report_path,
            answers_json,
            pending_triage: None,
        });
    }

    eprintln!("scrutiny probe: skip AI — findings-init from scan");
    let (_fr, findings_path) = run_findings_init(FindingsInitInput {
        cwd: cwd.clone(),
        scan_path: scan_path.clone(),
        eval_path: Some(eval_path.clone()),
        pack_path: Some(pack_path.clone()),
        plan_path: Some(plan_path.clone()),
        pr: pr_for_init,
        pr_summary: None,
    })?;
    if input.skip_triage {
        let pending = PendingTriage {
            findings_path: findings_path.clone(),
            cwd: cwd.clone(),
            client: Some(detected.clone()),
            model: plan.model.clone(),
            event: input.event.clone(),
            non_interactive: input.non_interactive,
            pack_path: pack_path.clone(),
            client_override: None,
        };
        let _ = eval;
        return Ok(ReviewResult {
            findings_path,
            report_path,
            answers_json,
            pending_triage: Some(pending),
        });
    }
    finish_triage_and_post(
        &findings_path,
        &cwd,
        Some(&detected),
        &plan.model,
        input.event.clone(),
        input.non_interactive,
        &pack_path,
        None,
    )?;
    let _ = eval;
    Ok(ReviewResult {
        findings_path,
        report_path,
        answers_json,
        pending_triage: None,
    })
}

/// Resume from an AI `review-report.json`: init findings → merge → triage → post.
/// No agent CLI / spawn prompts — report already exists.
pub fn run_review_from_report(input: ReportResumeInput) -> Result<(PathBuf, Option<PathBuf>)> {
    let cwd = input.cwd.clone();
    let text = std::fs::read_to_string(&input.report_path)
        .with_context(|| format!("read report {}", input.report_path.display()))?;
    let report: ReviewReport =
        serde_json::from_str(&text).context("parse ReviewReport (need findings array)")?;
    if report.findings.is_empty() {
        eprintln!(
            "scrutiny probe: warn: report has 0 findings ({})",
            input.report_path.display()
        );
    }

    let pr_refs = resolve_pr_refs(&cwd, input.pr.as_deref())?;
    let pr_arg = pr_refs.number.or(input.pr.clone());

    let findings_path = if let Some(scan_path) = &input.scan_path {
        eprintln!(
            "scrutiny probe: --from-report + scan {}",
            scan_path.display()
        );
        let (_fr, path) = run_findings_init(FindingsInitInput {
            cwd: cwd.clone(),
            scan_path: scan_path.clone(),
            eval_path: None,
            pack_path: None,
            plan_path: None,
            pr: pr_arg.clone(),
            pr_summary: None,
        })?;
        path
    } else {
        eprintln!("scrutiny probe: --from-report (empty findings shell, AI only)");
        let (_fr, path) = run_findings_init_empty(&cwd, pr_arg.as_deref())?;
        path
    };

    merge_ai_findings(&findings_path, &report.findings)?;
    promote_summary_concerns(&findings_path)?;
    eprintln!(
        "scrutiny probe: merged {} AI findings → {}",
        report.findings.len(),
        findings_path.display()
    );

    // Link PR early (correct head oid for snippets + post). Prompt if not found.
    if let Some(pr) = pr_arg.as_deref() {
        attach_pr_to_findings(&findings_path, &cwd, Some(pr))?;
    } else if input.non_interactive {
        attach_pr_to_findings(&findings_path, &cwd, None)?;
    } else {
        let fr = prompt_pr_if_missing(&findings_path, &cwd)?;
        if let Some(n) = fr.pr_number {
            eprintln!("scrutiny probe: linked PR #{n}");
        }
    }

    let model = if report.model.is_empty() {
        String::new()
    } else {
        report.model.clone()
    };
    let pack_placeholder = PathBuf::from("(none)");
    finish_triage_and_post(
        &findings_path,
        &cwd,
        None,
        &model,
        input.event,
        input.non_interactive,
        &pack_placeholder,
        input.client.clone(),
    )?;
    Ok((findings_path, Some(input.report_path)))
}

fn spawn_pr_summary_agent(
    enabled: bool,
    client: &crate::runtime::DetectedClient,
    model: &str,
    pack_path: &Path,
    cwd: &Path,
    term: Option<crate::terminal::ResolvedTerminal>,
) -> Option<JoinHandle<Result<(ProbePrSummary, PathBuf)>>> {
    if !enabled {
        return None;
    }
    let client = client.clone();
    let model = model.to_string();
    let pack = pack_path.to_path_buf();
    let cwd = cwd.to_path_buf();
    Some(thread::spawn(move || {
        run_pr_summary_agent(&client, &model, &pack, &cwd, term.as_ref())
    }))
}

fn join_pr_summary_agent(
    handle: Option<JoinHandle<Result<(ProbePrSummary, PathBuf)>>>,
) -> Option<ProbePrSummary> {
    let handle = handle?;
    match handle.join() {
        Ok(Ok((summary, path))) => {
            eprintln!("scrutiny probe: pr summary → {}", path.display());
            Some(summary)
        }
        Ok(Err(e)) => {
            eprintln!("scrutiny probe: warn: pr summary agent: {e:#}");
            None
        }
        Err(_) => {
            eprintln!("scrutiny probe: warn: pr summary thread panicked");
            None
        }
    }
}

fn finish_triage_and_post(
    findings_path: &Path,
    cwd: &Path,
    client: Option<&crate::runtime::DetectedClient>,
    model: &str,
    event: Option<String>,
    non_interactive: bool,
    pack_path: &Path,
    client_override: Option<String>,
) -> Result<()> {
    if non_interactive {
        eprintln!(
            "scrutiny probe: non-interactive — skip triage/post (edit findings JSON manually)"
        );
        return Ok(());
    }

    let pack_hint = pack_path.display().to_string();
    let mut ask = TriageAskCtx {
        client,
        model,
        client_override,
        pack_hint: &pack_hint,
    };
    let (report, _) = run_findings_triage(findings_path, Some(cwd), Some(&mut ask), false)?;
    let selected = report
        .findings
        .iter()
        .filter(|f| f.include == Some(true))
        .count();
    if selected == 0 {
        eprintln!("scrutiny probe: no findings selected — nothing to post.");
        return Ok(());
    }
    run_findings_resolve(findings_path, cwd, false)?;
    run_findings_validate(findings_path)?;

    let has_pr = {
        let report = prompt_pr_if_missing(findings_path, cwd)?;
        report.pr_number.is_some()
    };
    if !has_pr {
        eprintln!("scrutiny probe: no PR — skip post-comments. Open a PR or re-run with --pr.");
        return Ok(());
    }

    let (result, post_path) = run_post_comments(PostCommentsInput {
        findings_path: findings_path.to_path_buf(),
        cwd: cwd.to_path_buf(),
        strict: false,
        event,
        accept_suggested: false,
    })?;
    eprintln!(
        "scrutiny probe: posted {} comments → {}",
        result.posted_comments,
        post_path.display()
    );
    if let Some(url) = &result.html_url {
        eprintln!("  {url}");
    }
    Ok(())
}
