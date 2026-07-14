//! Orchestrate end-to-end `scrutiny review` (script-driven).

use anyhow::{bail, Context, Result};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::agent_runner::{
    build_ask_prompt, run_headless, run_isolated_review, run_team_review,
    session_records_from_report, HeadlessKind, AGENT_WALL_SECS,
};
use crate::config::{ensure_config, find_shipped_default, load_config};
use crate::eval::{run_eval, EvalInput};
use crate::findings::{
    merge_ai_findings, run_findings_init, run_findings_resolve, run_findings_triage,
    run_findings_validate, run_post_comments, FindingsInitInput, PostCommentsInput,
};
use crate::map::run_map;
use crate::pack::run_pack;
use crate::paths::write_json_pretty;
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
}

pub fn run_review(input: ReviewCmdInput) -> Result<(PathBuf, Option<PathBuf>)> {
    let cwd = input.cwd.clone();
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
        finish_triage_and_post(
            &findings_path,
            &cwd,
            &detected,
            &plan.model,
            input.event.clone(),
            input.non_interactive,
            &pack_path,
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
        &detected,
        &plan.model,
        input.event.clone(),
        input.non_interactive,
        &pack_path,
    )?;
    let _ = eval;
    Ok((findings_path, report_path))
}

fn finish_triage_and_post(
    findings_path: &Path,
    cwd: &Path,
    client: &crate::runtime::DetectedClient,
    model: &str,
    event: Option<String>,
    non_interactive: bool,
    pack_path: &Path,
) -> Result<()> {
    if non_interactive {
        eprintln!("scrutiny review: non-interactive — skip triage/post (edit findings JSON manually)");
        return Ok(());
    }

    run_findings_triage(findings_path)?;
    run_findings_resolve(findings_path, cwd, false)?;
    run_findings_validate(findings_path)?;

    let has_pr = {
        let report: crate::findings::FindingsReport =
            serde_json::from_str(&std::fs::read_to_string(findings_path)?)?;
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

    concern_loop(findings_path, cwd, client, model, pack_path)?;
    Ok(())
}

fn concern_loop(
    findings_path: &Path,
    cwd: &Path,
    client: &crate::runtime::DetectedClient,
    model: &str,
    pack_path: &Path,
) -> Result<()> {
    let report: crate::findings::FindingsReport =
        serde_json::from_str(&std::fs::read_to_string(findings_path)?)?;
    loop {
        eprintln!();
        eprint!("Concern / clarification? Enter finding id (e.g. F2) or empty to quit: ");
        let _ = io::stderr().flush();
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .context("read concern")?;
        let id = line.trim();
        if id.is_empty() {
            break;
        }
        let Some(f) = report.findings.iter().find(|f| f.id.eq_ignore_ascii_case(id)) else {
            eprintln!("unknown finding {id}");
            continue;
        };
        eprint!("Your question: ");
        let _ = io::stderr().flush();
        let mut q = String::new();
        io::stdin().read_line(&mut q).context("read question")?;
        let q = q.trim();
        if q.is_empty() {
            continue;
        }
        let context = format!(
            "Finding {} ({:?}:{:?}): {}\n{}\nProposed fix: {}\nPack: {}",
            f.id,
            f.anchor.path,
            f.anchor.line,
            f.title,
            f.explanation,
            f.proposed_fix,
            pack_path.display()
        );
        let prompt = build_ask_prompt(&context, q);
        let out = run_headless(
            client,
            model,
            cwd,
            &prompt,
            HeadlessKind::Ask,
            &format!("ask-{id}"),
            Duration::from_secs(AGENT_WALL_SECS),
        )?;
        if out.code != 0 && !out.timed_out {
            eprintln!("ask agent failed: {}", out.stderr);
            continue;
        }
        // Prefer printable result
        let answer = extract_text_answer(&out.stdout);
        if answer.trim().is_empty() {
            eprintln!("ask agent empty answer: {}", out.stderr);
            continue;
        }
        eprintln!("\n--- answer ---\n{answer}\n--------------");

        if report.pr_number.is_some() {
            eprint!("Post follow-up as PR comment? [y/N]: ");
            let _ = io::stderr().flush();
            let mut yn = String::new();
            io::stdin().read_line(&mut yn)?;
            if yn.trim().eq_ignore_ascii_case("y") {
                post_pr_comment(cwd, &report, &format!("**Re {}:**\n\n{answer}\n\n[AI Agent]", f.id))?;
            }
        }
    }
    Ok(())
}

fn extract_text_answer(stdout: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) {
        if let Some(r) = v.get("result").and_then(|x| x.as_str()) {
            return r.to_string();
        }
    }
    stdout.trim().to_string()
}

fn post_pr_comment(
    cwd: &Path,
    report: &crate::findings::FindingsReport,
    body: &str,
) -> Result<()> {
    let pr = report.pr_number.context("no pr")?;
    let (owner, name) = split_repo(&report.repo)?;
    let payload = serde_json::json!({ "body": body });
    let path = crate::paths::temp_artifact_path(&report.repo, "followup", "comment");
    write_json_pretty(&path, &payload)?;
    let endpoint = format!("repos/{owner}/{name}/issues/{pr}/comments");
    let output = Command::new("gh")
        .args(["api", "--method", "POST", &endpoint, "--input"])
        .arg(&path)
        .current_dir(cwd)
        .output()
        .context("gh api post comment")?;
    if !output.status.success() {
        bail!(
            "post follow-up failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    eprintln!("follow-up comment posted");
    Ok(())
}

fn split_repo(repo: &str) -> Result<(String, String)> {
    let parts: Vec<_> = repo.split('/').collect();
    if parts.len() >= 2 {
        return Ok((parts[parts.len() - 2].into(), parts[parts.len() - 1].into()));
    }
    bail!("repo must be owner/name, got {repo}");
}

fn resolve_pr_refs(
    cwd: &Path,
    pr: Option<&str>,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let Some(pr) = pr else {
        return Ok((None, None, None));
    };
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            pr,
            "--json",
            "baseRefName,headRefOid,url,number",
        ])
        .current_dir(cwd)
        .output()
        .context("gh pr view")?;
    if !output.status.success() {
        bail!(
            "gh pr view failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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
        .or_else(|| Some(pr.to_string()));
    Ok((base, head, num))
}
