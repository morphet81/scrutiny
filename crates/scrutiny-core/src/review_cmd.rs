//! Orchestrate end-to-end `scrutiny review` (script-driven).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::agent_runner::{
    run_isolated_review, run_team_review, session_records_from_report, ReviewReport,
};
use crate::config::{ensure_config, find_shipped_default, load_config};
use crate::eval::{run_eval, EvalInput};
use crate::findings::{
    attach_pr_to_findings, merge_ai_findings, prompt_pr_if_missing, run_findings_init,
    run_findings_init_empty, run_findings_resolve, run_findings_triage, run_findings_validate,
    run_post_comments, FindingsInitInput, PostCommentsInput, TriageAskCtx,
};
use crate::map::run_map;
use crate::pack::run_pack;
use crate::plan::{run_plan_confirm, run_plan_write, PlanConfirmInput, PlanWriteInput};
use crate::review_session::{run_review_session_write, ReviewSessionWriteInput};
use crate::runtime::{resolve_client, resolve_spawn_mode, ResolveClientInput};
use crate::scan::run_scan;

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

pub fn run_review(input: ReviewCmdInput) -> Result<(PathBuf, Option<PathBuf>)> {
    let cwd = input.cwd.clone();
    let hints: Vec<&Path> = input
        .from_report
        .as_deref()
        .into_iter()
        .chain(input.scan_path.as_deref())
        .collect();
    crate::paths::prepare_artifacts(&cwd, input.pr.as_deref(), &hints)?;

    if let Some(report_path) = input.from_report.clone() {
        return run_review_from_report(ReportResumeInput {
            report_path,
            cwd: input.cwd,
            pr: input.pr,
            event: input.event,
            non_interactive: input.non_interactive,
            scan_path: input.scan_path,
            client: input.client,
        });
    }

    let shipped = find_shipped_default(
        &std::env::current_exe().unwrap_or_else(|_| cwd.clone()),
    );
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

    let (base, head, pr_for_init) = resolve_pr_refs(&cwd, input.pr.as_deref())?;
    if let Some(ref prn) = pr_for_init {
        if let Some(n) = crate::paths::parse_pr_number(prn) {
            crate::paths::init_artifact_ctx(&cwd, &n.to_string())?;
        }
    }

    eprintln!("scrutiny review: eval…");
    let (eval, eval_path) = run_eval(EvalInput {
        cwd: cwd.clone(),
        head: head.clone(),
        base: base.clone(),
        client: Some(detected.client.clone()),
    })?;
    eprintln!("  {}", eval_path.display());

    eprintln!("scrutiny review: map…");
    let (_map, map_path) = run_map(&eval_path, &cwd)?;
    eprintln!("  {}", map_path.display());

    eprintln!("scrutiny review: pack…");
    let (_pack, pack_path) = run_pack(&map_path, &cwd)?;
    eprintln!("  {}", pack_path.display());

    eprintln!("scrutiny review: scan…");
    let (_scan, scan_path) = run_scan(&map_path, Some(&pack_path), Some(&eval_path), &cwd)?;
    eprintln!("  {}", scan_path.display());

    eprintln!("scrutiny review: plan-confirm…");
    let (answers, answers_path) = run_plan_confirm(PlanConfirmInput {
        eval_path: eval_path.clone(),
        client: Some(detected.client.clone()),
        spawn_mode: Some(spawn_mode.clone()),
        from_json: input.from_json.clone(),
    })?;
    eprintln!("  {}", answers_path.display());

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
    eprintln!("scrutiny review: plan {}", plan_path.display());

    let mut report_path: Option<PathBuf> = None;

    if !plan.skip_ai && !input.skip_agents {
        let (report, rpath) = if plan.spawn_mode == "team" {
            eprintln!("scrutiny review: team lead agent…");
            run_team_review(&detected, &plan, &pack_path, &cwd)?
        } else {
            eprintln!("scrutiny review: isolated parallel agents…");
            run_isolated_review(&detected, &plan, &pack_path, &cwd)?
        };
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
            Err(e) => eprintln!("scrutiny review: warn: review-session-write: {e:#}"),
        }

        eprintln!("scrutiny review: findings-init…");
        let (_fr, findings_path) = run_findings_init(FindingsInitInput {
            cwd: cwd.clone(),
            scan_path: scan_path.clone(),
            eval_path: Some(eval_path.clone()),
            pack_path: Some(pack_path.clone()),
            plan_path: Some(plan_path.clone()),
            pr: pr_for_init.clone(),
        })?;
        merge_ai_findings(&findings_path, &report.findings)?;
        eprintln!(
            "scrutiny review: merged {} AI findings → {}",
            report.findings.len(),
            findings_path.display()
        );
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
        return Ok((findings_path, report_path));
    }

    eprintln!("scrutiny review: skip AI — findings-init from scan");
    let (_fr, findings_path) = run_findings_init(FindingsInitInput {
        cwd: cwd.clone(),
        scan_path: scan_path.clone(),
        eval_path: Some(eval_path.clone()),
        pack_path: Some(pack_path.clone()),
        plan_path: Some(plan_path.clone()),
        pr: pr_for_init,
    })?;
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
    Ok((findings_path, report_path))
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
            "scrutiny review: warn: report has 0 findings ({})",
            input.report_path.display()
        );
    }

    let (_base, _head, pr_for_init) = resolve_pr_refs(&cwd, input.pr.as_deref())?;
    let pr_arg = pr_for_init.or(input.pr.clone());
    if let Some(ref prn) = pr_arg {
        if let Some(n) = crate::paths::parse_pr_number(prn) {
            crate::paths::init_artifact_ctx(&cwd, &n.to_string())?;
        }
    }

    let findings_path = if let Some(scan_path) = &input.scan_path {
        eprintln!(
            "scrutiny review: --from-report + scan {}",
            scan_path.display()
        );
        let (_fr, path) = run_findings_init(FindingsInitInput {
            cwd: cwd.clone(),
            scan_path: scan_path.clone(),
            eval_path: None,
            pack_path: None,
            plan_path: None,
            pr: pr_arg.clone(),
        })?;
        path
    } else {
        eprintln!("scrutiny review: --from-report (empty findings shell, AI only)");
        let (_fr, path) = run_findings_init_empty(&cwd, pr_arg.as_deref())?;
        path
    };

    merge_ai_findings(&findings_path, &report.findings)?;
    eprintln!(
        "scrutiny review: merged {} AI findings → {}",
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
            eprintln!("scrutiny review: linked PR #{n}");
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
        eprintln!("scrutiny review: non-interactive — skip triage/post (edit findings JSON manually)");
        return Ok(());
    }

    let pack_hint = pack_path.display().to_string();
    let mut ask = TriageAskCtx {
        client,
        model,
        client_override,
        pack_hint: &pack_hint,
    };
    run_findings_triage(findings_path, Some(cwd), Some(&mut ask))?;
    run_findings_resolve(findings_path, cwd, false)?;
    run_findings_validate(findings_path)?;

    let has_pr = {
        let report = prompt_pr_if_missing(findings_path, cwd)?;
        report.pr_number.is_some()
    };
    if !has_pr {
        eprintln!("scrutiny review: no PR — skip post-comments. Open a PR or re-run with --pr.");
        return Ok(());
    }

    let (result, post_path) = run_post_comments(PostCommentsInput {
        findings_path: findings_path.to_path_buf(),
        cwd: cwd.to_path_buf(),
        strict: false,
        event,
    })?;
    eprintln!(
        "scrutiny review: posted {} comments → {}",
        result.posted_comments,
        post_path.display()
    );
    if let Some(url) = &result.html_url {
        eprintln!("  {url}");
    }
    Ok(())
}

fn resolve_pr_refs(
    cwd: &Path,
    pr: Option<&str>,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let mut args = vec![
        "pr".into(),
        "view".into(),
        "--json".into(),
        "baseRefName,headRefOid,url,number".into(),
    ];
    if let Some(pr) = pr {
        args.insert(2, pr.to_string());
    }
    let output = Command::new("gh")
        .args(&args)
        .current_dir(cwd)
        .output()
        .context("gh pr view")?;
    if !output.status.success() {
        if pr.is_some() {
            bail!(
                "gh pr view failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // No --pr and no PR for current branch — resume can prompt later
        return Ok((None, None, None));
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let base = v
        .get("baseRefName")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let head = v
        .get("headRefOid")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let num = v
        .get("number")
        .and_then(|x| x.as_u64())
        .map(|n| n.to_string())
        .or_else(|| pr.map(|s| s.to_string()));
    Ok((base, head, num))
}
