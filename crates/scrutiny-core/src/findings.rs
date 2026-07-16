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
    let line = f.line.filter(|&l| l > 0);
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
            side: "RIGHT".into(),
            start_line: f.start_line.filter(|&l| l > 0).or(line),
            line,
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
    let path = {
        let p = a.path.trim();
        if p.is_empty() {
            None
        } else {
            Some(p.to_string())
        }
    };
    let line = if a.line > 0 { Some(a.line) } else { None };
    let start_line = a.start_line.filter(|&l| l > 0).or(line);
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
        paths: path.iter().cloned().collect(),
        anchor: Anchor {
            path: path.clone(),
            side: "RIGHT".into(),
            start_line,
            line,
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

/// Best-effort attach PR metadata onto a findings report (for --from-report resume).
pub fn attach_pr_to_findings(
    findings_path: &Path,
    cwd: &Path,
    pr: Option<&str>,
) -> Result<FindingsReport> {
    let mut report: FindingsReport = read_json(findings_path)?;
    if report.pr_number.is_some() {
        return Ok(report);
    }
    let (pr_number, pr_url, head_from_pr) = resolve_pr(cwd, pr)?;
    if let Some(n) = pr_number {
        report.pr_number = Some(n);
        report.pr_url = pr_url;
        report.mode = "pr".into();
        if let Some(h) = head_from_pr {
            report.head_oid = resolve_oid(cwd, &h)?;
        }
        write_json_pretty(findings_path, &report)?;
    }
    Ok(report)
}

/// Prompt for PR number/URL when missing (interactive). Writes findings JSON if provided.
pub fn prompt_pr_if_missing(
    findings_path: &Path,
    cwd: &Path,
) -> Result<FindingsReport> {
    use std::io::{self, Write};
    let report = attach_pr_to_findings(findings_path, cwd, None)?;
    if report.pr_number.is_some() {
        return Ok(report);
    }
    eprint!("No PR linked. Enter PR number/URL (empty = skip post): ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("read PR")?;
    let pr = line.trim();
    if pr.is_empty() {
        return Ok(report);
    }
    attach_pr_to_findings(findings_path, cwd, Some(pr))
}

/// Findings shell with zero scan findings (resume from AI report without `--scan`).
pub fn run_findings_init_empty(
    cwd: &Path,
    pr: Option<&str>,
) -> Result<(FindingsReport, PathBuf)> {
    let repo_ctx = crate::git::discover_repo(cwd)?;
    let (pr_number, pr_url, head_from_pr) = resolve_pr(cwd, pr)?;
    let head_oid = resolve_oid(cwd, head_from_pr.as_deref().unwrap_or("HEAD"))?;
    let repo = normalize_repo_name(&repo_ctx.repo_slug, cwd)?;

    let report = FindingsReport {
        version: 1,
        repo: repo.clone(),
        mode: if pr_number.is_some() {
            "pr".into()
        } else {
            "local".into()
        },
        pr_number,
        pr_url,
        head_oid,
        base_ref: String::new(),
        eval_path: None,
        pack_path: None,
        scan_path: None,
        plan_path: None,
        review: ReviewMeta::default(),
        findings: Vec::new(),
    };

    let out = temp_artifact_path(&repo, &repo_ctx.branch, "findings");
    write_json_pretty(&out, &report)?;
    Ok((report, out))
}

fn renumber(report: &mut FindingsReport) {
    for (i, f) in report.findings.iter_mut().enumerate() {
        let n = (i + 1) as u32;
        f.number = n;
        f.id = format!("F{n}");
    }
}

/// Optional ask hooks for triage (agent clarify before Post/Ignore).
pub struct TriageAskCtx<'a> {
    pub client: Option<&'a crate::runtime::DetectedClient>,
    pub model: &'a str,
    pub client_override: Option<String>,
    pub pack_hint: &'a str,
}

/// One finding triage decision (menu or line fallback).
enum TriagePick {
    Post,
    Option(usize),
    Ignore,
    Ask(String),
}

/// Interactive triage: Post/Ignore/Ask per finding.
/// TTY: arrow-key menu (Ask is an explicit item — never free-text on same prompt).
/// Non-TTY: letter line; ask only via `ask <question>` or multi-word free text (never bare P/I/A…).
/// Order: critical → warning → suggestion, then renumber F1…
pub fn run_findings_triage(
    findings_path: &Path,
    cwd: Option<&Path>,
    ask: Option<&mut TriageAskCtx<'_>>,
) -> Result<(FindingsReport, PathBuf)> {
    let mut ask = ask;
    let mut report: FindingsReport = read_json(findings_path)?;
    if report.findings.is_empty() {
        eprintln!("scrutiny findings-triage: no findings");
        return Ok((report, findings_path.to_path_buf()));
    }

    report.findings.sort_by(|a, b| {
        severity_rank_triage(&a.severity)
            .cmp(&severity_rank_triage(&b.severity))
            .then_with(|| a.number.cmp(&b.number))
    });
    renumber(&mut report);

    let (n_crit, n_warn, n_sug) = {
        let mut c = 0u32;
        let mut w = 0u32;
        let mut s = 0u32;
        for f in &report.findings {
            match normalize_severity(&f.severity).as_str() {
                "critical" => c += 1,
                "warning" => w += 1,
                _ => s += 1,
            }
        }
        (c, w, s)
    };
    eprintln!(
        "{}scrutiny findings-triage:{} {} findings ({} critical, {} warning, {} suggestion) — critical first.",
        style_bold(),
        style_reset(),
        report.findings.len(),
        n_crit,
        n_warn,
        n_sug
    );
    eprintln!("↑/↓ select Post / Ignore / Ask (or a fix option), Enter confirm.\n");

    let color = want_color();
    let mut last_sev = String::new();
    let mut resolved_client: Option<crate::runtime::DetectedClient> = None;
    let head_oid = report.head_oid.clone();
    let n = report.findings.len();

    for idx in 0..n {
        loop {
            let snapshot = report.findings.clone();
            let f = &mut report.findings[idx];
            let sev = normalize_severity(&f.severity);
            f.severity = sev.clone();
            if sev != last_sev {
                eprintln!();
                match sev.as_str() {
                    "critical" => {
                        eprintln!("{}## Critical{}", style_sev("critical", color), style_reset())
                    }
                    "warning" => {
                        eprintln!("{}## Warning{}", style_sev("warning", color), style_reset())
                    }
                    _ => {
                        eprintln!(
                            "{}## Suggestion{}",
                            style_sev("suggestion", color),
                            style_reset()
                        )
                    }
                }
                last_sev = sev.clone();
            }

            print_finding_block(f, cwd, &head_oid, &snapshot, color);

            let pick = match prompt_finding_decision(f) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("  triage prompt failed: {e:#}");
                    continue;
                }
            };

            match pick {
                TriagePick::Ignore => {
                    f.include = Some(false);
                    f.status = "skipped".into();
                    break;
                }
                TriagePick::Post => {
                    f.include = Some(true);
                    f.comment_body = Some(format!(
                        "**{}**\n\n{}\n\n**Fix:** {}\n\n{}",
                        f.title, f.explanation, f.proposed_fix, AI_AGENT_TAG
                    ));
                    f.status = "ready".into();
                    break;
                }
                TriagePick::Option(opt_i) => {
                    let text = f.fix_options[opt_i].clone();
                    f.include = Some(true);
                    f.chosen_option = Some(text.clone());
                    f.comment_body = Some(format!(
                        "**{}**\n\n{}\n\n**Fix:** {}\n\n{}",
                        f.title, f.explanation, text, AI_AGENT_TAG
                    ));
                    f.status = "ready".into();
                    break;
                }
                TriagePick::Ask(question) => {
                    let Some(ask_ctx) = ask.as_mut() else {
                        eprintln!("  (ask not available here — pick Post / Ignore / option)");
                        continue;
                    };
                    let pack_hint = ask_ctx.pack_hint.to_string();
                    let model = ask_ctx.model.to_string();
                    let override_cli = ask_ctx.client_override.clone();
                    let client_ref = ask_ctx.client.cloned();

                    let client = if let Some(c) = client_ref {
                        c
                    } else if let Some(c) = resolved_client.clone() {
                        c
                    } else {
                        let cwd_path = cwd.unwrap_or_else(|| Path::new("."));
                        match resolve_ask_client(cwd_path, override_cli.as_deref()) {
                            Ok(c) => {
                                resolved_client = Some(c.clone());
                                c
                            }
                            Err(e) => {
                                eprintln!("  ask needs agent CLI: {e:#}");
                                continue;
                            }
                        }
                    };

                    let ask_model = if model.is_empty() {
                        match client.client.as_str() {
                            "claude" => "sonnet",
                            "codex" => "o4-mini",
                            _ => "composer-2-fast",
                        }
                    } else {
                        model.as_str()
                    };

                    let context = format!(
                        "Finding {} (`{:?}`:{:?})\nTitle: {}\nWhy: {}\nFix: {}\nOptions: {:?}\nPack: {}\n\n\
                         When revising anchors: path+line MUST stay on a line present in the PR/pack unified diff \
                         (GitHub review comment attachable). Prefer an added (+) line. Do not invent out-of-diff lines.",
                        f.id,
                        f.anchor.path,
                        f.anchor.line,
                        f.title,
                        f.explanation,
                        f.proposed_fix,
                        f.fix_options,
                        pack_hint
                    );
                    let prompt =
                        crate::agent_runner::build_ask_revise_prompt(&context, &question);
                    let out = crate::agent_runner::run_headless(
                        &client,
                        ask_model,
                        cwd.unwrap_or_else(|| Path::new(".")),
                        &prompt,
                        crate::agent_runner::HeadlessKind::Ask,
                        &format!("ask-{}", f.id),
                        std::time::Duration::from_secs(crate::agent_runner::AGENT_WALL_SECS),
                    )?;
                    if out.code != 0 && !out.timed_out {
                        eprintln!("  ask agent failed: {}", out.stderr);
                        continue;
                    }
                    let answer = extract_ask_text(&out.stdout);
                    if answer.trim().is_empty() {
                        eprintln!("  ask empty: {}", out.stderr);
                        continue;
                    }
                    apply_ask_revision(f, &answer);
                    eprintln!("  (updated — decide again for {})", f.id);
                    // loop re-shows this finding only
                }
            }
        }
    }

    write_json_pretty(findings_path, &report)?;
    Ok((report, findings_path.to_path_buf()))
}

fn prompt_finding_decision(f: &TriageFinding) -> Result<TriagePick> {
    use std::io::{self, IsTerminal};
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        prompt_finding_decision_menu(f)
    } else {
        prompt_finding_decision_line(f)
    }
}

fn prompt_finding_decision_menu(f: &TriageFinding) -> Result<TriagePick> {
    use dialoguer::{theme::ColorfulTheme, Input, Select};

    let mut labels: Vec<String> = Vec::new();
    let mut kinds: Vec<&'static str> = Vec::new();
    let mut option_idxs: Vec<usize> = Vec::new();

    if f.fix_options.is_empty() {
        labels.push("Post".into());
        kinds.push("post");
        option_idxs.push(0);
    } else {
        for (i, opt) in f.fix_options.iter().enumerate() {
            labels.push(format!(
                "{}) {}",
                (b'A' + i as u8) as char,
                truncate(opt, 100)
            ));
            kinds.push("option");
            option_idxs.push(i);
        }
    }
    labels.push("Ignore".into());
    kinds.push("ignore");
    option_idxs.push(0);
    labels.push("Ask a question…".into());
    kinds.push("ask");
    option_idxs.push(0);

    let sel = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Decision")
        .items(&labels)
        .default(0)
        .interact()
        .context("triage menu")?;

    match kinds[sel] {
        "post" => Ok(TriagePick::Post),
        "option" => Ok(TriagePick::Option(option_idxs[sel])),
        "ignore" => Ok(TriagePick::Ignore),
        "ask" => {
            let q: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Your question")
                .allow_empty(false)
                .interact_text()
                .context("triage ask input")?;
            let q = q.trim().to_string();
            if q.is_empty() {
                bail!("empty question");
            }
            Ok(TriagePick::Ask(q))
        }
        _ => bail!("internal menu kind"),
    }
}

/// Non-TTY fallback: letters for actions; `ask <q>` or multi-word free text for ask.
/// Never treat single reserved letter (P/I/A…) as ask.
fn prompt_finding_decision_line(f: &TriageFinding) -> Result<TriagePick> {
    use std::io::{self, Write};

    eprintln!();
    if !f.fix_options.is_empty() {
        eprint!(
            "{}Option letter / I=Ignore / ask <question>:{} ",
            style_bold(),
            style_reset()
        );
    } else {
        eprint!(
            "{}P=Post / I=Ignore / ask <question>:{} ",
            style_bold(),
            style_reset()
        );
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
        return Ok(TriagePick::Ignore);
    }

    if !f.fix_options.is_empty() {
        if choice.len() == 1 {
            let c = choice.chars().next().unwrap().to_ascii_uppercase();
            if c >= 'A' && c < (b'A' + f.fix_options.len() as u8) as char {
                return Ok(TriagePick::Option((c as u8 - b'A') as usize));
            }
            if c == 'P' || c == 'Y' {
                eprintln!("  pick option letter A…, Ignore, or: ask <question>");
                return prompt_finding_decision_line(f);
            }
            eprintln!("  unknown letter — pick A…, I, or: ask <question>");
            return prompt_finding_decision_line(f);
        }
    } else if choice.eq_ignore_ascii_case("p")
        || choice.eq_ignore_ascii_case("post")
        || choice.eq_ignore_ascii_case("y")
    {
        return Ok(TriagePick::Post);
    }

    let question = if let Some(rest) = choice
        .strip_prefix("ask ")
        .or_else(|| choice.strip_prefix("ask:"))
        .or_else(|| {
            if choice.eq_ignore_ascii_case("ask") {
                Some("")
            } else {
                None
            }
        })
    {
        let rest = rest.trim();
        if rest.is_empty() {
            eprint!("  question: ");
            let _ = io::stderr().flush();
            let mut q = String::new();
            io::stdin().read_line(&mut q).context("read ask question")?;
            q.trim().to_string()
        } else {
            rest.to_string()
        }
    } else if choice.len() > 1
        && !choice.eq_ignore_ascii_case("post")
        && !choice.eq_ignore_ascii_case("ignore")
    {
        choice.to_string()
    } else {
        eprintln!("  use P/I (or option letter), or: ask <question>");
        return prompt_finding_decision_line(f);
    };

    if question.is_empty() {
        eprintln!("  empty question — try again");
        return prompt_finding_decision_line(f);
    }
    Ok(TriagePick::Ask(question))
}


fn print_finding_block(
    f: &TriageFinding,
    cwd: Option<&Path>,
    head_oid: &str,
    snapshot: &[TriageFinding],
    color: bool,
) {
    let where_ = f
        .anchor
        .path
        .as_deref()
        .or(f.paths.first().map(|s| s.as_str()))
        .unwrap_or("?");
    let line = f.anchor.line.filter(|&l| l > 0);
    let loc = match line {
        Some(l) => format!("{where_}:{l}"),
        None if where_ != "?" => format!("{where_} (file)"),
        None => "(global)".into(),
    };
    let sev = normalize_severity(&f.severity);

    eprintln!();
    eprintln!(
        "{}{}{} [{}{}{}] {}{}{} (`{}{}{}`)",
        style_bold(),
        f.id,
        style_reset(),
        style_sev(&sev, color),
        sev,
        style_reset(),
        style_bold(),
        f.title,
        style_reset(),
        style_dim(color),
        loc,
        style_reset(),
    );
    eprintln!("  Why: {}", truncate(&f.explanation, 240));
    if !f.fix_options.is_empty() {
        for (i, opt) in f.fix_options.iter().enumerate() {
            eprintln!("  {}) {}", (b'A' + i as u8) as char, truncate(opt, 140));
        }
    } else {
        eprintln!("  Fix: {}", truncate(&f.proposed_fix, 180));
    }
    let snippet = snippet_for_finding(cwd, head_oid, f, snapshot);
    if !snippet.is_empty() {
        eprintln!("{}", style_dim(color));
        eprintln!("{snippet}");
        eprintln!("{}", style_reset());
    }
}

fn resolve_ask_client(
    cwd: &Path,
    client_override: Option<&str>,
) -> Result<crate::runtime::DetectedClient> {
    let shipped = crate::config::find_shipped_default(
        &std::env::current_exe().unwrap_or_else(|_| cwd.to_path_buf()),
    );
    let cfg_path = crate::config::ensure_config(&shipped)?;
    let cfg = crate::config::load_config(&cfg_path)?;
    crate::runtime::resolve_client(
        &cfg,
        crate::runtime::ResolveClientInput {
            cli_override: client_override.map(|s| s.to_string()),
            skip_prompt: true,
        },
    )
}

fn extract_ask_text(stdout: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(stdout) {
        if let Some(r) = v.get("result").and_then(|x| x.as_str()) {
            return r.to_string();
        }
    }
    stdout.trim().to_string()
}

fn apply_ask_revision(f: &mut TriageFinding, answer: &str) {
    // Prefer JSON blob with revised fields
    let json_slice = extract_json_object(answer).unwrap_or(answer);
    if let Ok(v) = serde_json::from_str::<Value>(json_slice) {
        if let Some(t) = v.get("title").and_then(|x| x.as_str()) {
            if !t.is_empty() {
                f.title = t.to_string();
            }
        }
        if let Some(e) = v.get("explanation").and_then(|x| x.as_str()) {
            if !e.is_empty() {
                f.explanation = e.to_string();
            }
        }
        if let Some(p) = v.get("proposed_fix").and_then(|x| x.as_str()) {
            if !p.is_empty() {
                f.proposed_fix = p.to_string();
            }
        }
        if let Some(arr) = v.get("fix_options").and_then(|x| x.as_array()) {
            f.fix_options = arr
                .iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect();
        }
        if let Some(path) = v.get("path").and_then(|x| x.as_str()) {
            if !path.is_empty() {
                f.anchor.path = Some(path.to_string());
                f.paths = vec![path.to_string()];
            }
        }
        if let Some(line) = v.get("line").and_then(|x| x.as_u64()) {
            if line > 0 {
                f.anchor.line = Some(line as u32);
            }
        }
        f.chosen_option = None;
        f.include = None;
        f.comment_body = None;
        f.status = "pending".into();
        return;
    }
    // Plain text → append to explanation
    f.explanation = format!("{}\n\nClarification:\n{}", f.explanation, answer.trim());
    f.include = None;
    f.comment_body = None;
    f.status = "pending".into();
}

fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end > start {
        Some(&s[start..=end])
    } else {
        None
    }
}

fn want_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    // stderr isatty — crude check via is_terminal if available; else env
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

fn style_reset() -> &'static str {
    if want_color() {
        "\x1b[0m"
    } else {
        ""
    }
}
fn style_bold() -> &'static str {
    if want_color() {
        "\x1b[1m"
    } else {
        ""
    }
}
fn style_dim(on: bool) -> &'static str {
    if on {
        "\x1b[2m"
    } else {
        ""
    }
}
fn style_sev(sev: &str, on: bool) -> &'static str {
    if !on {
        return "";
    }
    match sev {
        "critical" => "\x1b[1;31m", // bold red
        "warning" => "\x1b[1;33m",  // bold yellow
        "suggestion" => "\x1b[1;36m", // bold cyan
        _ => "\x1b[1m",
    }
}

fn snippet_for_finding(
    cwd: Option<&Path>,
    head_oid: &str,
    f: &TriageFinding,
    _all: &[TriageFinding],
) -> String {
    let Some(cwd) = cwd else {
        return String::new();
    };
    let path = match f
        .anchor
        .path
        .as_deref()
        .or(f.paths.first().map(|s| s.as_str()))
    {
        Some(p) if !p.is_empty() => p,
        _ => return "(global — no file snippet)".into(),
    };
    let Ok(text) = git_show_file(cwd, head_oid, path) else {
        return format!("(could not read `{path}` at {head_oid})");
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let line = f.anchor.line.filter(|&l| l > 0);
    let mut note = String::new();
    let (start, end) = if let Some(l) = line {
        let idx = (l as usize).saturating_sub(1);
        if idx >= lines.len() {
            note = format!(
                "\n  (line {l} past end of file at this oid — {n} lines; showing head)",
                n = lines.len()
            );
            (0, 12.min(lines.len()))
        } else {
            let start = idx.saturating_sub(3);
            let end = (idx + 4).min(lines.len());
            (start, end.max(start))
        }
    } else if let Some(s) = f.anchor.start_line.filter(|&l| l > 0) {
        let idx = (s as usize).saturating_sub(1);
        if idx >= lines.len() {
            note = format!(
                "\n  (start_line {s} past end — {n} lines; showing head)",
                n = lines.len()
            );
            (0, 12.min(lines.len()))
        } else {
            let end = (idx + 12).min(lines.len());
            (idx, end.max(idx))
        }
    } else {
        note = "\n  (file-level finding — no single line)".into();
        (0, 12.min(lines.len()))
    };

    let mut out = String::from("```\n");
    for (i, l) in lines[start..end].iter().enumerate() {
        let n = start + i + 1;
        let mark = if line == Some(n as u32) { ">" } else { " " };
        out.push_str(&format!("{mark}{n:>4} | {l}\n"));
    }
    out.push_str("```");
    out.push_str(&note);
    if line.is_none() && note.is_empty() {
        out.push_str("\n  (file-level finding — no single line)");
    }
    out
}

fn severity_rank_triage(s: &str) -> u8 {
    match normalize_severity(s).as_str() {
        "critical" => 0,
        "warning" => 1,
        "suggestion" => 2,
        _ => 3,
    }
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

    // Prefer PR head oid when posting against a PR
    if let Some(pr) = report.pr_number {
        if let Ok((owner, name)) = split_repo(&report.repo) {
            if let Ok(Some(head)) = fetch_pr_head_oid(cwd, &owner, &name, pr) {
                report.head_oid = head;
            }
        }
    }
    report.head_oid = resolve_oid(cwd, &report.head_oid)?;

    let pr_patches: Option<std::collections::HashMap<String, String>> =
        if let Some(pr) = report.pr_number {
            if let Ok((owner, name)) = split_repo(&report.repo) {
                match fetch_pr_file_patches(cwd, &owner, &name, pr) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        eprintln!("scrutiny findings-resolve: warn: PR files fetch failed: {e:#}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

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
            if line == 0 {
                // Treat as no-line (file-level / global path) — do not fake resolve
                f.anchor.line = None;
                f.anchor.line_resolved = false;
                f.fail_reason = None;
                continue;
            }
            if line as usize > lines.len() {
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
                    f.fail_reason = Some("line_text mismatch; updated to actual".into());
                }
            }
            f.anchor.line = Some(line);
            f.anchor.line_text = Some(text);
            f.anchor.line_resolved = true;

            let in_diff = if let Some(patches) = &pr_patches {
                patches
                    .get(&path)
                    .map(|p| line_in_unified_diff(p, line))
                    .unwrap_or(false)
            } else {
                line_in_pack_diff(pack.as_ref(), &path, line)
            };
            f.anchor.in_diff = Some(in_diff);
            if !in_diff {
                f.fail_reason = Some(format!(
                    "line {line} not in PR/pack diff for `{path}` — will post as file comment if included"
                ));
                // Keep line for display; in_diff false → file demotion at post
            } else if f.fail_reason.as_deref() == Some("line_text mismatch; updated to actual") {
                // keep mismatch note
            } else {
                f.fail_reason = None;
            }
        } else {
            f.anchor.line = None;
            f.anchor.line_resolved = false;
            f.fail_reason = None; // file-level or global OK
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
        // Out-of-diff / unresolved line → file comment at post (path required for that).
        // No hard fail here — only need path or global.
        let has_path = f
            .anchor
            .path
            .as_ref()
            .or(f.paths.first())
            .map(|p| !p.is_empty())
            .unwrap_or(false);
        if !has_path {
            // global body OK
        }
    }

    // pr_number optional here — post-comments / review orchestrator gate on it

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
        review_body.push_str("\n\n### Global notes\n");
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

    if let Some(ref pending) = pending {
        let pending_drafts =
            list_pending_review_comments(&input.cwd, &owner, &name, pr, pending.id)?;
        eprintln!(
            "scrutiny post-comments: pending review #{} — {} draft comment(s):",
            pending.id,
            pending_drafts.len()
        );
        for c in &pending_drafts {
            let path = c.get("path").and_then(|p| p.as_str()).unwrap_or("?");
            if let Some(line) = c.get("line").and_then(|l| l.as_u64()) {
                eprintln!("  - {path}:{line}");
            } else {
                eprintln!("  - {path} (file)");
            }
        }
        let choice = {
            use std::io::{self, IsTerminal, Write};
            if io::stdin().is_terminal() && io::stderr().is_terminal() {
                use dialoguer::{theme::ColorfulTheme, Select};
                let items = [
                    "Add findings to pending review, then submit (drafts kept)",
                    "Submit pending as-is, then post findings as a separate review",
                ];
                let sel = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Pending review")
                    .items(&items)
                    .default(0)
                    .interact()
                    .context("pending review menu")?;
                if sel == 0 {
                    "1".to_string()
                } else {
                    "2".to_string()
                }
            } else {
                eprintln!(
                    "  1) Add findings to pending review, then submit (drafts kept)"
                );
                eprintln!(
                    "  2) Submit pending as-is, then post findings as a separate review"
                );
                eprint!("Enter 1 or 2: ");
                let _ = io::stderr().flush();
                read_stdin_line()?
            }
        };
        match choice.trim() {
            "1" => {
                event = resolve_review_event(&mut report, &input)?;
                report.review.event = Some(event.clone());
                write_json_pretty(&input.findings_path, &report)?;
                let (r, p) = finish_pending_with_comments(
                    &input.cwd,
                    &owner,
                    &name,
                    pr,
                    pending,
                    &event,
                    &review_body,
                    &api_comments,
                )?;
                resp = r;
                posted = p;
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
                    pending.id,
                    &close_event,
                    None,
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
    let failed: Vec<String> = Vec::new();

    for f in report.findings.iter_mut() {
        if f.include != Some(true) {
            continue;
        }
        let body = ensure_ai_tag(f.comment_body.as_deref().unwrap_or(""));
        f.comment_body = Some(body.clone());

        let path = f
            .anchor
            .path
            .clone()
            .or_else(|| f.paths.first().cloned())
            .filter(|p| !p.is_empty());
        let line = f.anchor.line.filter(|&l| l > 0);
        let line_usable = f.anchor.line_resolved
            && line.is_some()
            && path.is_some()
            && f.anchor.in_diff != Some(false);

        if line_usable {
            let mut c = json!({
                "path": path,
                "side": f.anchor.side,
                "line": line,
                "body": body,
            });
            if let Some(start) = f.anchor.start_line {
                if let Some(end) = line {
                    if start > 0 && start < end {
                        c["start_line"] = json!(start);
                        c["start_side"] = json!(f.anchor.side);
                    }
                }
            }
            api_comments.push(c);
            f.status = "ready".into();
            f.fail_reason = None;
            continue;
        }

        if let Some(path) = path {
            // Path but no usable inline line (missing, unresolved, or not on PR patch)
            // → file-level review comment (clear line for payload / status)
            if line.is_some() {
                eprintln!(
                    "scrutiny post-comments: {} line not on PR/pack diff — posting as file comment on `{path}`",
                    f.id
                );
                f.anchor.line = None;
                f.anchor.start_line = None;
                f.anchor.line_resolved = false;
                f.anchor.in_diff = Some(false);
            }
            api_comments.push(json!({
                "path": path,
                "body": body,
                "subject_type": "file",
            }));
            f.status = "ready_file".into();
            f.fail_reason = None;
            continue;
        }

        // Global — review body
        body_fallbacks.push(format_fallback_bullet(f, &body));
        f.status = "posted_body".into();
        f.fail_reason = None;
        if strict && f.severity == "critical" {
            bail!("strict: critical {} has no path for file/line comment", f.id);
        }
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
    // Create a PENDING review with only the summary body and no inline comments.
    // All findings are added as independent threads via GraphQL so each appears
    // as its own comment (not bundled into the review body text).
    let (pending_resp, _) = post_pull_request_review(
        cwd,
        owner,
        name,
        pr,
        &report.head_oid,
        review_body,
        None,
        &[],
    )?;
    let review_id = pending_resp
        .get("id")
        .and_then(|i| i.as_u64())
        .context("new pending review missing id")?;
    let node_id = pending_resp
        .get("node_id")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .context("new pending review missing node_id")?;
    let pending = PendingReview { id: review_id, node_id };
    finish_pending_with_comments(cwd, owner, name, pr, &pending, event, review_body, api_comments)
}

/// POST create-review. `event = None` leaves the review PENDING.
fn post_pull_request_review(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
    commit_id: &str,
    body: &str,
    event: Option<&str>,
    line_comments: &[Value],
) -> Result<(Value, u32)> {
    let endpoint = format!("repos/{owner}/{name}/pulls/{pr}/reviews");
    let payload_path = temp_artifact_path("scrutiny", "review", "payload");

    let try_post = |comments: &[Value]| -> Result<(Value, bool)> {
        let mut payload = json!({
            "commit_id": commit_id,
            "body": body,
            "comments": comments,
        });
        if let Some(ev) = event {
            payload["event"] = json!(ev);
        }
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
            return Ok((resp, true));
        }
        let err = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        if err.contains("pending review") || stdout.contains("pending review") {
            bail!(
                "gh api failed due to a pending review. Re-run post-comments — it will ask how to handle it.\n{err} {stdout}"
            );
        }
        Ok((
            json!({
                "stderr": err.to_string(),
                "stdout": stdout.to_string(),
            }),
            false,
        ))
    };

    let (resp, ok) = try_post(line_comments)?;
    if ok {
        return Ok((resp, line_comments.len() as u32));
    }

    let stripped: Vec<Value> = line_comments
        .iter()
        .map(|c| {
            let mut c = c.clone();
            if let Some(obj) = c.as_object_mut() {
                obj.remove("start_line");
                obj.remove("start_side");
            }
            c
        })
        .collect();
    let had_multiline = line_comments.iter().any(|c| c.get("start_line").is_some());
    if had_multiline {
        eprintln!("scrutiny post-comments: retry without start_line/start_side…");
        let (resp2, ok2) = try_post(&stripped)?;
        if ok2 {
            return Ok((resp2, stripped.len() as u32));
        }
        let err = resp2
            .get("stderr")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let stdout = resp2
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        bail!(
            "gh api review failed (after stripping multilines). No body dump — fix anchors / PR head and re-run.\n{err}\n{stdout}\npayload: {}",
            payload_path.display()
        );
    }

    let err = resp
        .get("stderr")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stdout = resp
        .get("stdout")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    bail!(
        "gh api review failed. No body dump — line comments must attach on the PR diff.\n{err}\n{stdout}\npayload: {}",
        payload_path.display()
    );
}

struct PendingReview {
    id: u64,
    node_id: String,
}

fn find_pending_review(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
) -> Result<Option<PendingReview>> {
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
            let id = r
                .get("id")
                .and_then(|i| i.as_u64())
                .context("pending review missing id")?;
            let node_id = r
                .get("node_id")
                .and_then(|n| n.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .with_context(|| {
                    format!("pending review #{id} missing node_id (needed for GraphQL append)")
                })?;
            return Ok(Some(PendingReview { id, node_id }));
        }
    }
    Ok(None)
}

/// Append findings onto an existing PENDING review via GraphQL, then submit it.
/// Existing draft comments stay on GitHub (no delete/recreate).
fn finish_pending_with_comments(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
    pending: &PendingReview,
    event: &str,
    review_body: &str,
    api_comments: &[Value],
) -> Result<(Value, u32)> {
    eprintln!(
        "scrutiny post-comments: appending {} comment(s) to pending #{} via GraphQL…",
        api_comments.len(),
        pending.id
    );
    for (i, c) in api_comments.iter().enumerate() {
        add_pending_review_thread(cwd, &pending.node_id, c).with_context(|| {
            format!(
                "failed appending comment {}/{} to pending #{}",
                i + 1,
                api_comments.len(),
                pending.id
            )
        })?;
    }
    eprintln!(
        "scrutiny post-comments: submitting pending #{} as {event}…",
        pending.id
    );
    let resp = submit_review_event(
        cwd,
        owner,
        name,
        pr,
        pending.id,
        event,
        Some(review_body),
    )?;
    let posted = api_comments.len() as u32;
    eprintln!(
        "scrutiny post-comments: append ok — {posted} comment(s) submitted as {event}"
    );
    Ok((resp, posted))
}

const ADD_THREAD_MUTATION: &str = r#"
mutation($input: AddPullRequestReviewThreadInput!) {
  addPullRequestReviewThread(input: $input) {
    thread { id }
  }
}
"#;

fn add_pending_review_thread(
    cwd: &Path,
    review_node_id: &str,
    comment: &Value,
) -> Result<()> {
    let body = comment
        .get("body")
        .and_then(|b| b.as_str())
        .filter(|s| !s.is_empty())
        .context("comment body required")?;
    let path = comment
        .get("path")
        .and_then(|p| p.as_str())
        .filter(|s| !s.is_empty())
        .context("comment path required")?;

    let is_file = comment.get("subject_type").and_then(|s| s.as_str()) == Some("file")
        || comment.get("line").and_then(|l| l.as_u64()).unwrap_or(0) == 0;

    let mut input = json!({
        "pullRequestReviewId": review_node_id,
        "body": body,
        "path": path,
    });

    if is_file {
        input["subjectType"] = json!("FILE");
    } else {
        let line = comment
            .get("line")
            .and_then(|l| l.as_u64())
            .context("line comment missing line")?;
        input["subjectType"] = json!("LINE");
        input["line"] = json!(line);
        input["side"] = json!(comment
            .get("side")
            .and_then(|s| s.as_str())
            .unwrap_or("RIGHT"));
        if let Some(start) = comment.get("start_line").and_then(|l| l.as_u64()) {
            if start > 0 && start < line {
                input["startLine"] = json!(start);
                if let Some(ss) = comment.get("start_side").and_then(|s| s.as_str()) {
                    input["startSide"] = json!(ss);
                }
            }
        }
    }

    let try_add = |input: &Value| -> Result<()> {
        let data = gh_graphql(cwd, ADD_THREAD_MUTATION, &json!({ "input": input }))?;
        if data
            .pointer("/addPullRequestReviewThread/thread/id")
            .and_then(|v| v.as_str())
            .is_none()
        {
            bail!("addPullRequestReviewThread returned no thread id: {data}");
        }
        Ok(())
    };

    match try_add(&input) {
        Ok(()) => Ok(()),
        Err(first_err) if input.get("startLine").is_some() => {
            eprintln!("scrutiny post-comments: GraphQL retry without startLine/startSide…");
            let mut stripped = input.clone();
            if let Some(obj) = stripped.as_object_mut() {
                obj.remove("startLine");
                obj.remove("startSide");
            }
            try_add(&stripped).with_context(|| format!("after multiline strip: {first_err:#}"))
        }
        Err(e) => Err(e),
    }
}

fn list_pending_review_comments(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
    review_id: u64,
) -> Result<Vec<Value>> {
    let endpoint = format!("repos/{owner}/{name}/pulls/{pr}/reviews/{review_id}/comments");
    let v = gh_json(cwd, &["api", &endpoint, "--paginate"])?;
    let Some(arr) = v.as_array() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for c in arr {
        let body = c.get("body").and_then(|b| b.as_str()).unwrap_or("");
        let path = c.get("path").and_then(|p| p.as_str()).unwrap_or("");
        if path.is_empty() || body.is_empty() {
            continue;
        }
        let mut draft = json!({
            "path": path,
            "body": body,
        });
        if let Some(line) = c.get("line").and_then(|l| l.as_u64()) {
            draft["line"] = json!(line);
            if let Some(side) = c.get("side").and_then(|s| s.as_str()) {
                draft["side"] = json!(side);
            } else {
                draft["side"] = json!("RIGHT");
            }
            if let Some(start) = c.get("start_line").and_then(|l| l.as_u64()) {
                if start > 0 && start < line {
                    draft["start_line"] = json!(start);
                    if let Some(ss) = c.get("start_side").and_then(|s| s.as_str()) {
                        draft["start_side"] = json!(ss);
                    }
                }
            }
        } else if c.get("subject_type").and_then(|s| s.as_str()) == Some("file")
            || c.get("line").is_none()
        {
            draft["subject_type"] = json!("file");
        } else {
            continue;
        }
        out.push(draft);
    }
    Ok(out)
}

fn submit_review_event(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
    review_id: u64,
    event: &str,
    body: Option<&str>,
) -> Result<Value> {
    let endpoint =
        format!("repos/{owner}/{name}/pulls/{pr}/reviews/{review_id}/events");
    let mut payload = json!({ "event": event });
    if let Some(b) = body.filter(|s| !s.is_empty()) {
        payload["body"] = json!(b);
    }
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

fn gh_graphql(cwd: &Path, query: &str, variables: &Value) -> Result<Value> {
    let payload = json!({
        "query": query,
        "variables": variables,
    });
    let payload_path = temp_artifact_path("scrutiny", "graphql", "payload");
    write_json_pretty(&payload_path, &payload)?;
    let output = Command::new("gh")
        .args(["api", "graphql", "--input"])
        .arg(&payload_path)
        .current_dir(cwd)
        .output()
        .context("run gh api graphql")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        bail!("gh api graphql failed: {stderr} {stdout}");
    }
    let resp: Value = serde_json::from_str(stdout.trim()).context("parse graphql resp")?;
    if let Some(errors) = resp.get("errors").and_then(|e| e.as_array()) {
        if !errors.is_empty() {
            let msgs: Vec<String> = errors
                .iter()
                .map(|e| {
                    e.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown")
                        .to_string()
                })
                .collect();
            bail!("graphql errors: {}", msgs.join("; "));
        }
    }
    Ok(resp.get("data").cloned().unwrap_or(Value::Null))
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
    use std::io::{self, IsTerminal};

    eprintln!("scrutiny post-comments: review action?");
    if !extra.is_empty() {
        eprintln!("{extra}");
    }

    let event = if io::stdin().is_terminal() && io::stderr().is_terminal() {
        use dialoguer::{theme::ColorfulTheme, Select};
        let items = [
            "COMMENT — comments only",
            "REQUEST_CHANGES — block the PR",
            "APPROVE — approve",
        ];
        let sel = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Review event")
            .items(&items)
            .default(0)
            .interact()
            .context("review event menu")?;
        match sel {
            0 => "COMMENT",
            1 => "REQUEST_CHANGES",
            _ => "APPROVE",
        }
        .to_string()
    } else {
        eprintln!("  1) COMMENT       — comments only");
        eprintln!("  2) REQUEST_CHANGES — block the PR");
        eprintln!("  3) APPROVE       — approve");
        eprint!("Enter 1, 2, 3 (or COMMENT / REQUEST_CHANGES / APPROVE): ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let choice = read_stdin_line()?;
        match choice.as_str() {
            "1" | "c" | "C" => "COMMENT".into(),
            "2" | "r" | "R" => "REQUEST_CHANGES".into(),
            "3" | "a" | "A" => "APPROVE".into(),
            other => other.to_string(),
        }
    };
    normalize_event(&event)
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
    let (crit, warn, sug) = included_counts(report);
    prompt_event_choice(&format!(
        "  Included to post: {crit} critical, {warn} warning, {sug} suggestion"
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
    let mut sug = 0u32;
    for f in &report.findings {
        if f.include != Some(true) {
            continue;
        }
        match normalize_severity(&f.severity).as_str() {
            "critical" => crit += 1,
            "warning" => warn += 1,
            _ => sug += 1,
        }
    }
    (crit, warn, sug)
}

fn format_fallback_bullet(f: &TriageFinding, body: &str) -> String {
    let path = f.anchor.path.as_deref().unwrap_or("?");
    let line = f.anchor.line.unwrap_or(0);
    format!("- **{}** (`{path}:{line}`) — {}\n{body}\n", f.title, f.severity)
}

fn default_review_body(report: &FindingsReport) -> String {
    let mut crit = 0u32;
    let mut warn = 0u32;
    let mut sug = 0u32;
    for f in &report.findings {
        if f.include != Some(true) {
            continue;
        }
        match normalize_severity(&f.severity).as_str() {
            "critical" => crit += 1,
            "warning" => warn += 1,
            _ => sug += 1,
        }
    }
    format!("Scrutiny review: {crit} critical, {warn} warning, {sug} suggestion.")
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
        return true; // unknown — allow attempt (PR patches should have been preferred)
    };
    let Some(slice) = pack.slices.iter().find(|s| s.path == path) else {
        return false;
    };
    if line_in_unified_diff(&slice.unified_diff, line) {
        return true;
    }
    // Also accept if inside a symbol slice range
    pack.slices
        .iter()
        .filter(|s| s.path == path)
        .flat_map(|s| s.symbol_slices.iter())
        .any(|sym| line as usize >= sym.start_line && line as usize <= sym.end_line)
}

/// True if `line` (1-based, new-file side) appears in a unified diff / PR file patch.
fn line_in_unified_diff(diff: &str, line: u32) -> bool {
    let mut new_line: u32 = 0;
    for diff_line in diff.lines() {
        if let Some(rest) = diff_line.strip_prefix("@@") {
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
    false
}

fn fetch_pr_file_patches(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
) -> Result<std::collections::HashMap<String, String>> {
    let endpoint = format!("repos/{owner}/{name}/pulls/{pr}/files?per_page=100");
    let output = Command::new("gh")
        .args(["api", "--paginate", &endpoint])
        .current_dir(cwd)
        .output()
        .context("gh api pull files")?;
    if !output.status.success() {
        bail!(
            "gh api pull files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let v: Value = serde_json::from_slice(&output.stdout).context("parse pull files")?;
    let mut map = std::collections::HashMap::new();
    let arr = if let Some(a) = v.as_array() {
        a.clone()
    } else {
        // paginate may concatenate — try as single array only
        bail!("unexpected pull files JSON shape");
    };
    for file in arr {
        let path = file
            .get("filename")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            continue;
        }
        if let Some(patch) = file.get("patch").and_then(|x| x.as_str()) {
            map.insert(path, patch.to_string());
        }
    }
    Ok(map)
}

fn fetch_pr_head_oid(
    cwd: &Path,
    owner: &str,
    name: &str,
    pr: u64,
) -> Result<Option<String>> {
    let endpoint = format!("repos/{owner}/{name}/pulls/{pr}");
    let output = Command::new("gh")
        .args(["api", &endpoint, "-q", ".head.sha"])
        .current_dir(cwd)
        .output()
        .context("gh api pull head")?;
    if !output.status.success() {
        return Ok(None);
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() || sha == "null" {
        Ok(None)
    } else {
        Ok(Some(sha))
    }
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
        assert_eq!(normalize_severity("low"), "suggestion");
        assert_eq!(normalize_severity("info"), "suggestion");
        assert_eq!(normalize_severity("critical"), "critical");
        assert_eq!(normalize_severity("suggestion"), "suggestion");
    }

    #[test]
    fn unified_diff_line_detect() {
        let patch = "@@ -10,3 +10,4 @@\n context\n-old\n+new\n more\n";
        // @@ +10 → context(10), +new(11), more(12)
        assert!(line_in_unified_diff(patch, 11));
        assert!(line_in_unified_diff(patch, 10));
        assert!(!line_in_unified_diff(patch, 99));
    }
}
