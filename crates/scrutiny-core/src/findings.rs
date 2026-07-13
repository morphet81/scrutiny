//! Findings artifact: triage JSON + resolve line anchors + post GitHub PR review.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::eval::EvalReport;
use crate::pack::PackReport;
use crate::paths::{temp_artifact_path, write_json_pretty};
use crate::scan::{normalize_severity, Finding as ScanFinding, ScanReport};

pub const AI_AGENT_TAG: &str = "[AI Agent]";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingsReport {
    pub version: u32,
    pub repo: String,
    pub mode: String,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub head_oid: String,
    pub base_ref: String,
    pub eval_path: Option<String>,
    pub pack_path: Option<String>,
    pub scan_path: Option<String>,
    pub plan_path: Option<String>,
    pub review: ReviewMeta,
    pub findings: Vec<TriageFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewMeta {
    pub event: Option<String>,
    pub body: Option<String>,
    pub posted: bool,
    pub review_id: Option<u64>,
    pub html_url: Option<String>,
}

impl Default for ReviewMeta {
    fn default() -> Self {
        Self {
            event: None,
            body: None,
            posted: false,
            review_id: None,
            html_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageFinding {
    pub id: String,
    pub number: u32,
    pub severity: String,
    pub title: String,
    pub explanation: String,
    pub proposed_fix: String,
    pub fix_options: Vec<String>,
    pub chosen_option: Option<String>,
    pub include: Option<bool>,
    pub source: String,
    pub paths: Vec<String>,
    pub anchor: Anchor,
    pub comment_body: Option<String>,
    pub status: String,
    pub fail_reason: Option<String>,
    /// Optional substring to locate line when number is wrong.
    #[serde(default)]
    pub needle: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anchor {
    pub path: Option<String>,
    pub side: String,
    pub start_line: Option<u32>,
    pub line: Option<u32>,
    pub line_resolved: bool,
    pub line_text: Option<String>,
    pub in_diff: Option<bool>,
}

impl Default for Anchor {
    fn default() -> Self {
        Self {
            path: None,
            side: "RIGHT".into(),
            start_line: None,
            line: None,
            line_resolved: false,
            line_text: None,
            in_diff: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FindingsInitInput {
    pub cwd: PathBuf,
    pub scan_path: PathBuf,
    pub eval_path: Option<PathBuf>,
    pub pack_path: Option<PathBuf>,
    pub plan_path: Option<PathBuf>,
    pub pr: Option<String>,
}

pub fn run_findings_init(input: FindingsInitInput) -> Result<(FindingsReport, PathBuf)> {
    let scan: ScanReport = read_json(&input.scan_path)?;
    let eval: Option<EvalReport> = if let Some(p) = &input.eval_path {
        Some(read_json(p)?)
    } else if let Some(p) = &scan.eval_path {
        let path = Path::new(p);
        if path.exists() {
            Some(read_json(path)?)
        } else {
            None
        }
    } else {
        None
    };

    let head_oid = eval
        .as_ref()
        .map(|e| e.head.clone())
        .unwrap_or_else(|| "HEAD".into());
    let base_ref = eval
        .as_ref()
        .map(|e| e.base.clone())
        .unwrap_or_default();
    let repo = eval
        .as_ref()
        .map(|e| e.repo.clone())
        .unwrap_or_else(|| scan.repo.clone());
    let mode = eval
        .as_ref()
        .map(|e| e.mode.clone())
        .unwrap_or_else(|| "local".into());

    let (pr_number, pr_url, head_from_pr) = resolve_pr(&input.cwd, input.pr.as_deref())?;
    let head_oid = head_from_pr.unwrap_or(head_oid);
    // Resolve symbolic HEAD to oid when needed
    let head_oid = resolve_oid(&input.cwd, &head_oid)?;

    let findings: Vec<TriageFinding> = scan
        .findings
        .iter()
        .enumerate()
        .map(|(i, f)| scan_to_triage(f, i + 1))
        .collect();

    let report = FindingsReport {
        version: 1,
        repo: normalize_repo_name(&repo, &input.cwd)?,
        mode: if pr_number.is_some() {
            "pr".into()
        } else {
            mode
        },
        pr_number,
        pr_url,
        head_oid,
        base_ref,
        eval_path: input
            .eval_path
            .map(|p| p.display().to_string())
            .or(scan.eval_path.clone()),
        pack_path: input
            .pack_path
            .map(|p| p.display().to_string())
            .or(scan.pack_path.clone()),
        scan_path: Some(input.scan_path.display().to_string()),
        plan_path: input.plan_path.map(|p| p.display().to_string()),
        review: ReviewMeta::default(),
        findings,
    };

    let out = temp_artifact_path(&report.repo, &scan.branch, "findings");
    write_json_pretty(&out, &report)?;
    Ok((report, out))
}

fn scan_to_triage(f: &ScanFinding, number: usize) -> TriageFinding {
    let path = f.paths.first().cloned();
    TriageFinding {
        id: format!("F{number}"),
        number: number as u32,
        severity: normalize_severity(&f.severity),
        title: f.title.clone(),
        explanation: f.explanation.clone(),
        proposed_fix: f.proposed_fix.clone(),
        fix_options: f.fix_options.clone(),
        chosen_option: None,
        include: None,
        source: if f.source.starts_with("scan.") {
            "scan".into()
        } else {
            f.source.clone()
        },
        paths: f.paths.clone(),
        anchor: Anchor {
            path,
            ..Anchor::default()
        },
        comment_body: None,
        status: "pending".into(),
        fail_reason: None,
        needle: None,
    }
}

/// Append curated AI findings into an existing findings report and rewrite IDs.
pub fn merge_ai_findings(
    findings_path: &Path,
    ai: &[crate::agent_runner::AgentFinding],
) -> Result<(FindingsReport, PathBuf)> {
    let mut report: FindingsReport = read_json(findings_path)?;
    let mut next = report.findings.len() + 1;
    for a in ai {
        report.findings.push(agent_to_triage(a, next));
        next += 1;
    }
    renumber(&mut report);
    write_json_pretty(findings_path, &report)?;
    Ok((report, findings_path.to_path_buf()))
}

fn agent_to_triage(a: &crate::agent_runner::AgentFinding, number: usize) -> TriageFinding {
    TriageFinding {
        id: format!("F{number}"),
        number: number as u32,
        severity: normalize_severity(&a.severity),
        title: a.title.clone(),
        explanation: a.explanation.clone(),
        proposed_fix: a.proposed_fix.clone(),
        fix_options: a.fix_options.clone(),
        chosen_option: None,
        include: None,
        source: format!("ai.{}", a.source_role),
        paths: vec![a.path.clone()],
        anchor: Anchor {
            path: Some(a.path.clone()),
            side: "RIGHT".into(),
            start_line: a.start_line,
            line: Some(a.line),
            line_resolved: false,
            line_text: None,
            in_diff: None,
        },
        comment_body: None,
        status: "pending".into(),
        fail_reason: None,
        needle: None,
    }
}

fn renumber(report: &mut FindingsReport) {
    for (i, f) in report.findings.iter_mut().enumerate() {
        let n = (i + 1) as u32;
        f.number = n;
        f.id = format!("F{n}");
    }
}

/// Interactive stdin triage: Post/Ignore (or fix option) per finding.
pub fn run_findings_triage(findings_path: &Path) -> Result<(FindingsReport, PathBuf)> {
    use std::io::{self, Write};
    let mut report: FindingsReport = read_json(findings_path)?;
    if report.findings.is_empty() {
        eprintln!("scrutiny findings-triage: no findings");
        return Ok((report, findings_path.to_path_buf()));
    }

    eprintln!("scrutiny findings-triage: decide Post/Ignore for each finding (one pass).");
    for f in &mut report.findings {
        let where_ = f
            .anchor
            .path
            .as_deref()
            .or(f.paths.first().map(|s| s.as_str()))
            .unwrap_or("?");
        let line = f.anchor.line.unwrap_or(0);
        eprintln!();
        eprintln!(
            "{} [{}] {} (`{}:{line}`)",
            f.id, f.severity, f.title, where_
        );
        eprintln!("  Why: {}", truncate(&f.explanation, 200));
        if !f.fix_options.is_empty() {
            for (i, opt) in f.fix_options.iter().enumerate() {
                eprintln!("  {}) {}", (b'A' + i as u8) as char, truncate(opt, 120));
            }
            eprint!("Choose option letter, or I to Ignore [I]: ");
        } else {
            eprintln!("  Fix: {}", truncate(&f.proposed_fix, 160));
            eprint!("Post (P) or Ignore (I) [I]: ");
        }
        let _ = io::stderr().flush();
        let mut line_in = String::new();
        io::stdin()
            .read_line(&mut line_in)
            .context("read triage choice")?;
        let choice = line_in.trim();
        if choice.is_empty()
            || choice.eq_ignore_ascii_case("i")
            || choice.eq_ignore_ascii_case("ignore")
        {
            f.include = Some(false);
            f.status = "skipped".into();
            continue;
        }
        if !f.fix_options.is_empty() {
            let idx = if choice.len() == 1 {
                let c = choice.chars().next().unwrap().to_ascii_uppercase();
                if c >= 'A' && c < (b'A' + f.fix_options.len() as u8) as char {
                    Some((c as u8 - b'A') as usize)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(i) = idx {
                f.include = Some(true);
                f.chosen_option = Some(f.fix_options[i].clone());
                f.comment_body = Some(format!(
                    "**{}**\n\n{}\n\n**Fix:** {}\n\n{}",
                    f.title, f.explanation, f.fix_options[i], AI_AGENT_TAG
                ));
                f.status = "ready".into();
            } else {
                f.include = Some(false);
                f.status = "skipped".into();
            }
        } else if choice.eq_ignore_ascii_case("p")
            || choice.eq_ignore_ascii_case("post")
            || choice.eq_ignore_ascii_case("y")
        {
            f.include = Some(true);
            f.comment_body = Some(format!(
                "**{}**\n\n{}\n\n**Fix:** {}\n\n{}",
                f.title, f.explanation, f.proposed_fix, AI_AGENT_TAG
            ));
            f.status = "ready".into();
        } else {
            f.include = Some(false);
            f.status = "skipped".into();
        }
    }

    write_json_pretty(findings_path, &report)?;
    Ok((report, findings_path.to_path_buf()))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

pub fn run_findings_resolve(findings_path: &Path, cwd: &Path, strict: bool) -> Result<(FindingsReport, PathBuf)> {
    let mut report: FindingsReport = read_json(findings_path)?;
    let pack: Option<PackReport> = if let Some(p) = &report.pack_path {
        let path = Path::new(p);
        if path.exists() {
            Some(read_json(path)?)
        } else {
            None
        }
    } else {
        None
    };

    report.head_oid = resolve_oid(cwd, &report.head_oid)?;

    let mut critical_fail = false;
    for f in &mut report.findings {
        if f.include == Some(false) {
            f.status = "skipped".into();
            continue;
        }
        // Resolve candidates: include true, or undecided (still try anchors for later)
        let Some(path) = f.anchor.path.clone().or_else(|| f.paths.first().cloned()) else {
            if f.include == Some(true) && f.severity == "critical" {
                critical_fail = true;
            }
            f.anchor.line_resolved = false;
            f.fail_reason = Some("no path for anchor".into());
            continue;
        };
        f.anchor.path = Some(path.clone());

        let file_text = git_show_file(cwd, &report.head_oid, &path);
        let Ok(file_text) = file_text else {
            f.anchor.line_resolved = false;
            f.fail_reason = Some(format!("git show {}:{} failed", report.head_oid, path));
            if f.include == Some(true) && f.severity == "critical" {
                critical_fail = true;
            }
            continue;
        };
        let lines: Vec<&str> = file_text.lines().collect();

        let mut resolved_line = f.anchor.line;
        if let Some(needle) = &f.needle {
            if let Some((idx, _)) = lines
                .iter()
                .enumerate()
                .find(|(_, l)| l.contains(needle.as_str()))
            {
                resolved_line = Some((idx + 1) as u32);
            }
        }

        if let Some(line) = resolved_line {
            if line == 0 || line as usize > lines.len() {
                f.anchor.line_resolved = false;
                f.fail_reason = Some(format!(
                    "line {line} out of range (file has {} lines)",
                    lines.len()
                ));
                if f.include == Some(true) && f.severity == "critical" {
                    critical_fail = true;
                }
                continue;
            }
            let text = lines[(line as usize) - 1].to_string();
            if let Some(expected) = &f.anchor.line_text {
                if expected.trim() != text.trim() {
                    // try needle / keep but mark mismatch
                    f.fail_reason = Some("line_text mismatch; updated to actual".into());
                }
            }
            f.anchor.line = Some(line);
            f.anchor.line_text = Some(text);
            f.anchor.line_resolved = true;
            f.fail_reason = None;
            f.anchor.in_diff = Some(line_in_pack_diff(pack.as_ref(), &path, line));
        } else {
            f.anchor.line_resolved = false;
            f.fail_reason = Some("no line set; provide anchor.line or needle".into());
            if f.include == Some(true) && f.severity == "critical" {
                critical_fail = true;
            }
        }

        if let (Some(start), Some(end)) = (f.anchor.start_line, f.anchor.line) {
            if start > end {
                f.anchor.line_resolved = false;
                f.fail_reason = Some("start_line > line".into());
            }
        }
    }

    write_json_pretty(findings_path, &report)?;
    if strict && critical_fail {
        bail!("findings-resolve: included critical finding(s) could not resolve line anchors");
    }
    Ok((report, findings_path.to_path_buf()))
}

pub fn run_findings_validate(findings_path: &Path) -> Result<(FindingsReport, PathBuf)> {
    let report: FindingsReport = read_json(findings_path)?;
    let mut errs = Vec::new();

    // review.event is prompted by post-comments — not required here

    for f in &report.findings {
        if f.include.is_none() {
            errs.push(format!("{}: include not decided", f.id));
            continue;
        }
        if f.include != Some(true) {
            continue;
        }
        if f.comment_body.as_ref().map(|s| s.trim().is_empty()).unwrap_or(true) {
            errs.push(format!("{}: comment_body required when include=true", f.id));
        }
        if !f.fix_options.is_empty() && f.chosen_option.is_none() {
            errs.push(format!(
                "{}: chosen_option required when fix_options non-empty",
                f.id
            ));
        }
        if !f.anchor.line_resolved || f.anchor.line.is_none() || f.anchor.path.is_none() {
            errs.push(format!(
                "{}: anchor not resolved (run findings-resolve)",
                f.id
            ));
        }
    }

    if report.pr_number.is_none() {
        errs.push(
            "pr_number missing — open a PR or re-run with --pr / findings-init --pr".into(),
        );
    }

    if !errs.is_empty() {
        bail!("findings-validate failed:\n  - {}", errs.join("\n  - "));
    }
    Ok((report, findings_path.to_path_buf()))
}

#[derive(Debug, Clone)]
pub struct PostCommentsInput {
    pub findings_path: PathBuf,
    pub cwd: PathBuf,
    pub strict: bool,
    /// If set, skip interactive prompt. Else prompt on stderr/stdin when review.event missing.
    pub event: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostResult {
    pub version: u32,
    pub findings_path: String,
    pub review_id: Option<u64>,
    pub html_url: Option<String>,
    pub event: String,
    pub posted_comments: u32,
    pub body_fallback_comments: u32,
    pub failed: Vec<String>,
}

pub fn run_post_comments(input: PostCommentsInput) -> Result<(PostResult, PathBuf)> {
    // Re-resolve then validate (findings triage completeness — not review.event)
    run_findings_resolve(&input.findings_path, &input.cwd, input.strict)?;
    let (mut report, _) = run_findings_validate(&input.findings_path)?;

    let pr = report
        .pr_number
        .context("pr_number required to post")?;
    let (owner, name) = split_repo(&report.repo)?;

    ensure_gh()?;

    let (api_comments, body_fallbacks, failed) =
        build_comment_payloads(&mut report, input.strict)?;

    let mut review_body = report
        .review
        .body
        .clone()
        .unwrap_or_else(|| default_review_body(&report));
    review_body = ensure_ai_tag(&review_body);
    if !body_fallbacks.is_empty() {
        review_body.push_str("\n\n### Could not attach as line comments\n");
        for b in &body_fallbacks {
            review_body.push_str(b);
            review_body.push('\n');
        }
    }

    // Pending review owned by current user?
    let pending = find_pending_review(&input.cwd, &owner, &name, pr)?;

    let event;
    let resp;
    let posted;

    if let Some(pending_id) = pending {
        eprintln!("scrutiny post-comments: pending review #{pending_id} found.");
        eprintln!("  1) Add comments to that pending review, then submit it");
        eprintln!("  2) Close the pending review first, then post a new review with these findings");
        eprint!("Enter 1 or 2: ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let choice = read_stdin_line()?;
        match choice.trim() {
            "1" => {
                add_comments_to_pending(
                    &input.cwd,
                    &owner,
                    &name,
                    pr,
                    pending_id,
                    &api_comments,
                )?;
                event = resolve_review_event(&mut report, &input)?;
                report.review.event = Some(event.clone());
                write_json_pretty(&input.findings_path, &report)?;
                // Update review body then submit
                set_pending_review_body(
                    &input.cwd,
                    &owner,
                    &name,
                    pr,
                    pending_id,
                    &review_body,
                )?;
                resp = submit_review_event(
                    &input.cwd,
                    &owner,
                    &name,
                    pr,
                    pending_id,
                    &event,
                )?;
                posted = api_comments.len() as u32;
            }
            "2" => {
                eprintln!("Close pending review as:");
                let close_event = prompt_event_choice(
                    "  (closing does not attach new findings to the old review)",
                )?;
                let _ = submit_review_event(
                    &input.cwd,
                    &owner,
                    &name,
                    pr,
                    pending_id,
                    &close_event,
                )?;
                eprintln!("Pending review closed as {close_event}.");
                event = resolve_review_event(&mut report, &input)?;
                report.review.event = Some(event.clone());
                if report.review.body.is_none() {
                    report.review.body = Some(default_review_body(&report));
                }
                write_json_pretty(&input.findings_path, &report)?;
                let (r, p) = create_new_review(
                    &input.cwd,
                    &owner,
                    &name,
                    pr,
                    &report,
                    &event,
                    &review_body,
                    &api_comments,
                )?;
                resp = r;
                posted = p;
            }
            other => bail!("expected 1 or 2, got {other}"),
        }
    } else {
        event = resolve_review_event(&mut report, &input)?;
        report.review.event = Some(event.clone());
        if report.review.body.is_none() {
            report.review.body = Some(default_review_body(&report));
        }
        write_json_pretty(&input.findings_path, &report)?;
        let (r, p) = create_new_review(
            &input.cwd,
            &owner,
            &name,
            pr,
            &report,
            &event,
            &review_body,
            &api_comments,
        )?;
        resp = r;
        posted = p;
    }

    for f in report.findings.iter_mut() {
        if f.include == Some(true) && f.status == "pending" {
            f.status = "posted".into();
        }
    }
    finalize_post(
        &mut report,
        &input.findings_path,
        resp,
        &event,
        posted,
        body_fallbacks.len() as u32,
        failed,
    )
}

fn build_comment_payloads(
    report: &mut FindingsReport,
    strict: bool,
) -> Result<(Vec<Value>, Vec<String>, Vec<String>)> {
    let mut api_comments: Vec<Value> = Vec::new();
    let mut body_fallbacks: Vec<String> = Vec::new();
    let mut failed = Vec::new();

    for f in report.findings.iter_mut() {
        if f.include != Some(true) {
            continue;
        }
        let body = ensure_ai_tag(f.comment_body.as_deref().unwrap_or(""));
        f.comment_body = Some(body.clone());

        if !f.anchor.line_resolved
            || f.anchor.path.is_none()
            || f.anchor.line.is_none()
            || f.anchor.in_diff == Some(false)
        {
            body_fallbacks.push(format_fallback_bullet(f, &body));
            f.status = "failed".into();
            f.fail_reason = Some("line not in diff or unresolved; appended to review body".into());
            failed.push(f.id.clone());
            if strict && f.severity == "critical" {
                bail!("strict: critical {} cannot post as line comment", f.id);
            }
            continue;
        }

        let mut c = json!({
            "path": f.anchor.path,
            "side": f.anchor.side,
            "line": f.anchor.line,
            "body": body,
        });
        if let Some(start) = f.anchor.start_line {
            if let Some(end) = f.anchor.line {
                if start < end {
                    c["start_line"] = json!(start);
                    c["start_side"] = json!(f.anchor.side);
                }
            }
        }
        api_comments.push(c);
    }
    Ok((api_comments, body_fallbacks, failed))
}

fn create_new_review(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
    report: &FindingsReport,
    event: &str,
    review_body: &str,
    api_comments: &[Value],
) -> Result<(Value, u32)> {
    let payload = json!({
        "commit_id": report.head_oid,
        "body": review_body,
        "event": event,
        "comments": api_comments,
    });
    let endpoint = format!("repos/{owner}/{name}/pulls/{pr}/reviews");
    let payload_path = temp_artifact_path(&report.repo, "review", "payload");
    write_json_pretty(&payload_path, &payload)?;

    let output = Command::new("gh")
        .args(["api", "--method", "POST", &endpoint, "--input"])
        .arg(&payload_path)
        .current_dir(cwd)
        .output()
        .context("run gh api POST review")?;

    if output.status.success() {
        let resp: Value =
            serde_json::from_slice(&output.stdout).context("parse review resp")?;
        return Ok((resp, api_comments.len() as u32));
    }

    let err = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // If pending-review error slipped through, tell user to re-run (script should have handled)
    if err.contains("pending review") || stdout.contains("pending review") {
        bail!(
            "gh api failed due to a pending review. Re-run post-comments — it will ask how to handle it.\n{err} {stdout}"
        );
    }

    // Fallback: body-only review
    if !api_comments.is_empty() {
        let mut body2 = review_body.to_string();
        body2.push_str("\n\n### Review comments (fallback)\n");
        for c in api_comments {
            let path = c.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let line = c.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
            let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
            body2.push_str(&format!("- `{path}:{line}`\n{body}\n"));
        }
        let payload2 = json!({
            "commit_id": report.head_oid,
            "body": body2,
            "event": event,
            "comments": [],
        });
        write_json_pretty(&payload_path, &payload2)?;
        let output2 = Command::new("gh")
            .args(["api", "--method", "POST", &endpoint, "--input"])
            .arg(&payload_path)
            .current_dir(cwd)
            .output()
            .context("run gh api POST review fallback")?;
        if !output2.status.success() {
            bail!(
                "gh api review failed: {} {}",
                String::from_utf8_lossy(&output2.stderr),
                String::from_utf8_lossy(&output2.stdout)
            );
        }
        let resp: Value =
            serde_json::from_slice(&output2.stdout).context("parse review resp")?;
        return Ok((resp, 0));
    }
    bail!("gh api review failed: {err} {stdout}");
}

fn find_pending_review(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
) -> Result<Option<u64>> {
    let login = gh_json(cwd, &["api", "user", "-q", ".login"])?
        .as_str()
        .unwrap_or("")
        .to_string();
    if login.is_empty() {
        return Ok(None);
    }
    let endpoint = format!("repos/{owner}/{name}/pulls/{pr}/reviews");
    let reviews = gh_json(cwd, &["api", &endpoint])?;
    let Some(arr) = reviews.as_array() else {
        return Ok(None);
    };
    for r in arr {
        let state = r.get("state").and_then(|s| s.as_str()).unwrap_or("");
        let user = r
            .pointer("/user/login")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        if state.eq_ignore_ascii_case("PENDING") && user == login {
            if let Some(id) = r.get("id").and_then(|i| i.as_u64()) {
                return Ok(Some(id));
            }
        }
    }
    Ok(None)
}

fn add_comments_to_pending(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
    review_id: u64,
    comments: &[Value],
) -> Result<()> {
    for c in comments {
        let endpoint =
            format!("repos/{owner}/{name}/pulls/{pr}/reviews/{review_id}/comments");
        let payload_path = temp_artifact_path("scrutiny", "pending", "comment");
        write_json_pretty(&payload_path, c)?;
        let output = Command::new("gh")
            .args(["api", "--method", "POST", &endpoint, "--input"])
            .arg(&payload_path)
            .current_dir(cwd)
            .output()
            .context("add comment to pending review")?;
        if !output.status.success() {
            bail!(
                "failed to add comment to pending review: {} {}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
    Ok(())
}

fn set_pending_review_body(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
    review_id: u64,
    body: &str,
) -> Result<()> {
    // GitHub allows updating pending review via PUT
    let endpoint = format!("repos/{owner}/{name}/pulls/{pr}/reviews/{review_id}");
    let payload = json!({ "body": body });
    let payload_path = temp_artifact_path("scrutiny", "pending", "body");
    write_json_pretty(&payload_path, &payload)?;
    let output = Command::new("gh")
        .args(["api", "--method", "PUT", &endpoint, "--input"])
        .arg(&payload_path)
        .current_dir(cwd)
        .output()
        .context("update pending review body")?;
    // Non-fatal if PUT fails — submit still works
    if !output.status.success() {
        eprintln!(
            "scrutiny post-comments: warn: could not update pending review body ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn submit_review_event(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
    review_id: u64,
    event: &str,
) -> Result<Value> {
    let endpoint =
        format!("repos/{owner}/{name}/pulls/{pr}/reviews/{review_id}/events");
    let payload = json!({ "event": event });
    let payload_path = temp_artifact_path("scrutiny", "pending", "event");
    write_json_pretty(&payload_path, &payload)?;
    let output = Command::new("gh")
        .args(["api", "--method", "POST", &endpoint, "--input"])
        .arg(&payload_path)
        .current_dir(cwd)
        .output()
        .context("submit review event")?;
    if !output.status.success() {
        bail!(
            "failed to submit review event: {} {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    serde_json::from_slice(&output.stdout).context("parse submit event resp")
}

fn gh_json(cwd: &Path, args: &[&str]) -> Result<Value> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("gh {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // -q may print raw string without quotes
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Ok(Value::Null);
    }
    if let Ok(v) = serde_json::from_str::<Value>(&text) {
        return Ok(v);
    }
    Ok(Value::String(text))
}

fn read_stdin_line() -> Result<String> {
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("read stdin")?;
    Ok(line.trim().to_string())
}

fn prompt_event_choice(extra: &str) -> Result<String> {
    eprintln!("scrutiny post-comments: review action?");
    if !extra.is_empty() {
        eprintln!("{extra}");
    }
    eprintln!("  1) COMMENT       — comments only");
    eprintln!("  2) REQUEST_CHANGES — block the PR");
    eprintln!("  3) APPROVE       — approve");
    eprint!("Enter 1, 2, 3 (or COMMENT / REQUEST_CHANGES / APPROVE): ");
    use std::io::Write;
    let _ = std::io::stderr().flush();
    let choice = read_stdin_line()?;
    let event = match choice.as_str() {
        "1" | "c" | "C" => "COMMENT",
        "2" | "r" | "R" => "REQUEST_CHANGES",
        "3" | "a" | "A" => "APPROVE",
        other => other,
    };
    normalize_event(event)
}

fn finalize_post(
    report: &mut FindingsReport,
    findings_path: &Path,
    resp: Value,
    event: &str,
    posted_comments: u32,
    body_fallback_comments: u32,
    failed: Vec<String>,
) -> Result<(PostResult, PathBuf)> {
    let review_id = resp.get("id").and_then(|v| v.as_u64());
    let html_url = resp
        .get("html_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    report.review.posted = true;
    report.review.review_id = review_id;
    report.review.html_url = html_url.clone();
    report.review.body = Some(ensure_ai_tag(
        report.review.body.as_deref().unwrap_or(""),
    ));
    write_json_pretty(findings_path, report)?;

    let result = PostResult {
        version: 1,
        findings_path: findings_path.display().to_string(),
        review_id,
        html_url,
        event: event.into(),
        posted_comments,
        body_fallback_comments,
        failed,
    };
    let out = temp_artifact_path(&report.repo, "post", "result");
    write_json_pretty(&out, &result)?;
    Ok((result, out))
}

fn resolve_review_event(report: &mut FindingsReport, input: &PostCommentsInput) -> Result<String> {
    if let Some(ev) = &input.event {
        return normalize_event(ev);
    }
    if let Some(ev) = &report.review.event {
        return normalize_event(ev);
    }
    let (crit, warn, info) = included_counts(report);
    prompt_event_choice(&format!(
        "  Included to post: {crit} critical, {warn} warning, {info} info"
    ))
}

fn normalize_event(raw: &str) -> Result<String> {
    match raw.trim().to_ascii_uppercase().replace('-', "_").as_str() {
        "COMMENT" | "COMMENTS" => Ok("COMMENT".into()),
        "REQUEST_CHANGES" | "REQUESTCHANGES" | "CHANGES" => Ok("REQUEST_CHANGES".into()),
        "APPROVE" | "APPROVED" => Ok("APPROVE".into()),
        other => bail!("invalid review event {other} (COMMENT|REQUEST_CHANGES|APPROVE)"),
    }
}

fn included_counts(report: &FindingsReport) -> (u32, u32, u32) {
    let mut crit = 0u32;
    let mut warn = 0u32;
    let mut info = 0u32;
    for f in &report.findings {
        if f.include != Some(true) {
            continue;
        }
        match f.severity.as_str() {
            "critical" => crit += 1,
            "warning" => warn += 1,
            _ => info += 1,
        }
    }
    (crit, warn, info)
}

fn format_fallback_bullet(f: &TriageFinding, body: &str) -> String {
    let path = f.anchor.path.as_deref().unwrap_or("?");
    let line = f.anchor.line.unwrap_or(0);
    format!("- **{}** (`{path}:{line}`) — {}\n{body}\n", f.title, f.severity)
}

fn default_review_body(report: &FindingsReport) -> String {
    let mut crit = 0u32;
    let mut warn = 0u32;
    let mut info = 0u32;
    for f in &report.findings {
        if f.include != Some(true) {
            continue;
        }
        match f.severity.as_str() {
            "critical" => crit += 1,
            "warning" => warn += 1,
            _ => info += 1,
        }
    }
    format!("Scrutiny review: {crit} critical, {warn} warning, {info} info.")
}

fn ensure_ai_tag(body: &str) -> String {
    let trimmed = body.trim_end();
    if trimmed.ends_with(AI_AGENT_TAG) {
        trimmed.to_string()
    } else if trimmed.is_empty() {
        AI_AGENT_TAG.to_string()
    } else {
        format!("{trimmed}\n\n{AI_AGENT_TAG}")
    }
}

fn line_in_pack_diff(pack: Option<&PackReport>, path: &str, line: u32) -> bool {
    let Some(pack) = pack else {
        return true; // unknown — allow attempt
    };
    let Some(slice) = pack.slices.iter().find(|s| s.path == path) else {
        return false;
    };
    // Parse unified diff new-file line numbers
    let mut new_line: u32 = 0;
    for diff_line in slice.unified_diff.lines() {
        if let Some(rest) = diff_line.strip_prefix("@@") {
            // @@ -a,b +c,d @@
            if let Some(plus) = rest.split('+').nth(1) {
                let start = plus
                    .split(|c: char| c == ',' || c == ' ')
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                new_line = start.saturating_sub(1);
            }
            continue;
        }
        if diff_line.starts_with("+++") || diff_line.starts_with("---") {
            continue;
        }
        if diff_line.starts_with('+') {
            new_line += 1;
            if new_line == line {
                return true;
            }
        } else if diff_line.starts_with('-') {
            // old only
        } else if diff_line.starts_with(' ') || diff_line.is_empty() {
            new_line += 1;
            if new_line == line {
                return true;
            }
        }
    }
    // Also accept if inside a symbol slice range
    pack.slices
        .iter()
        .filter(|s| s.path == path)
        .flat_map(|s| s.symbol_slices.iter())
        .any(|sym| line as usize >= sym.start_line && line as usize <= sym.end_line)
}

fn git_show_file(cwd: &Path, oid: &str, path: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["show", &format!("{oid}:{path}")])
        .current_dir(cwd)
        .output()
        .context("git show")?;
    if !output.status.success() {
        bail!(
            "git show {oid}:{path}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn resolve_oid(cwd: &Path, rev: &str) -> Result<String> {
    if rev.len() >= 40 && rev.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(rev.to_string());
    }
    let output = Command::new("git")
        .args(["rev-parse", rev])
        .current_dir(cwd)
        .output()
        .context("git rev-parse")?;
    if !output.status.success() {
        bail!(
            "git rev-parse {rev}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn resolve_pr(
    cwd: &Path,
    pr_arg: Option<&str>,
) -> Result<(Option<u64>, Option<String>, Option<String>)> {
    if !command_exists("gh") {
        return Ok((None, None, None));
    }
    let mut args = vec![
        "pr".into(),
        "view".into(),
        "--json".into(),
        "number,url,headRefOid".into(),
    ];
    if let Some(pr) = pr_arg {
        args.insert(2, pr.to_string());
    }
    let output = Command::new("gh").args(&args).current_dir(cwd).output();
    let Ok(output) = output else {
        return Ok((None, None, None));
    };
    if !output.status.success() {
        return Ok((None, None, None));
    }
    let v: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    let number = v.get("number").and_then(|n| n.as_u64());
    let url = v
        .get("url")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string());
    let head = v
        .get("headRefOid")
        .and_then(|h| h.as_str())
        .map(|s| s.to_string());
    Ok((number, url, head))
}

fn normalize_repo_name(repo: &str, cwd: &Path) -> Result<String> {
    if repo.contains('/') && !repo.contains(' ') {
        return Ok(repo.to_string());
    }
    if command_exists("gh") {
        let output = Command::new("gh")
            .args(["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"])
            .current_dir(cwd)
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !name.is_empty() {
                    return Ok(name);
                }
            }
        }
    }
    Ok(repo.to_string())
}

fn split_repo(repo: &str) -> Result<(String, String)> {
    let mut parts = repo.split('/');
    let owner = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("");
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        bail!("repo must be owner/name, got {repo}");
    }
    Ok((owner.into(), name.into()))
}

fn ensure_gh() -> Result<()> {
    if !command_exists("gh") {
        bail!("gh CLI not found — required to post PR review comments");
    }
    Ok(())
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_tag_appended() {
        assert_eq!(ensure_ai_tag("hi"), format!("hi\n\n{AI_AGENT_TAG}"));
        assert_eq!(
            ensure_ai_tag(&format!("hi\n\n{AI_AGENT_TAG}")),
            format!("hi\n\n{AI_AGENT_TAG}")
        );
    }

    #[test]
    fn severity_via_scan_norm() {
        assert_eq!(normalize_severity("high"), "critical");
        assert_eq!(normalize_severity("medium"), "warning");
        assert_eq!(normalize_severity("low"), "info");
        assert_eq!(normalize_severity("critical"), "critical");
    }
}
