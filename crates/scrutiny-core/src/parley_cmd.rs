//! End-to-end `scrutiny parley` orchestration.

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Input};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::agent_runner::{
    run_headless, run_nonheadless, wait_for_sentinels, HeadlessKind, AGENT_WALL_SECS,
};
use crate::config::{ensure_config, find_shipped_default, load_config, Config};
use crate::git::git_stdout;
use crate::parley::fetch::{run_parley_fetch, ParleyComment, ParleyCommentsFile, ParleyFetchInput};
use crate::parley::fixes::{
    init_fixes_file, load_fixes, merge_fix_entries, parse_fixes_from_agent_stdout, save_fixes,
    validate_fixes_complete, FixEntry,
};
use crate::parley::plan::{
    load_parley_comments, load_parley_plan, prompt_parley_answers, run_parley_plan_write,
    ParleyPlan, ParleyPlanWriteInput,
};
use crate::parley::reply::{run_parley_reply, ParleyReplyInput};
use crate::paths::{artifact_path, prepare_artifacts, write_json_pretty};
use crate::runtime::{resolve_client, ResolveClientInput};
use crate::terminal::{resolve_terminal, TerminalContext};

#[derive(Debug, Clone)]
pub struct ParleyCmdInput {
    pub cwd: PathBuf,
    pub pr: Option<String>,
    pub client: Option<String>,
    pub spawn_mode: Option<String>,
    pub from_json: Option<String>,
    pub non_interactive: bool,
    /// Skip headless agents (fetch + plan only; for tests / resume).
    pub skip_agents: bool,
    /// Skip commit/push/reply.
    pub skip_ship: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParleySessionSummary {
    pub version: u32,
    pub comments_path: String,
    pub plan_path: String,
    pub fixes_path: String,
    pub reply_path: Option<String>,
    pub comment_count: u32,
    pub members: u32,
    #[serde(default)]
    pub verifiers: u32,
    pub evangelists: u32,
    pub spawn_mode: String,
}

pub fn run_parley(input: ParleyCmdInput) -> Result<PathBuf> {
    let cwd = input.cwd.clone();
    prepare_artifacts(&cwd, input.pr.as_deref(), &[])?;

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

    eprintln!("scrutiny parley: fetch unresolved review threads…");
    let (comments, comments_path) = run_parley_fetch(ParleyFetchInput {
        cwd: cwd.clone(),
        pr: input.pr.clone(),
    })?;
    eprintln!(
        "  {} unresolved → {}",
        comments.comments.len(),
        comments_path.display()
    );

    if comments.comments.is_empty() {
        eprintln!("scrutiny parley: nothing to address — exit");
        let summary = ParleySessionSummary {
            version: 1,
            comments_path: comments_path.display().to_string(),
            plan_path: String::new(),
            fixes_path: String::new(),
            reply_path: None,
            comment_count: 0,
            members: 0,
            verifiers: 0,
            evangelists: 0,
            spawn_mode: String::new(),
        };
        let path = artifact_path("parley-session");
        write_json_pretty(&path, &summary)?;
        return Ok(path);
    }

    let model = cfg
        .models
        .get(&detected.client)
        .and_then(|m| m.m.clone().or(m.l.clone()).or(m.s.clone()))
        .unwrap_or_else(|| detected.client.clone());

    let answers = prompt_parley_answers(
        &cfg,
        &detected.client,
        &model,
        comments.comments.len(),
        input.spawn_mode.as_deref(),
        input.from_json.as_deref(),
        input.non_interactive,
    )?;

    let (plan, plan_path) = run_parley_plan_write(ParleyPlanWriteInput {
        comments: comments.clone(),
        comments_path: comments_path.clone(),
        answers,
        cfg: cfg.clone(),
    })?;
    eprintln!(
        "scrutiny parley: plan members={} verifiers={} evangelists={} mode={} → {}",
        plan.members,
        plan.verifiers,
        plan.evangelists,
        plan.spawn_mode,
        plan_path.display()
    );

    let fixes_path = PathBuf::from(&plan.fixes_path);
    init_fixes_file(&fixes_path, plan.pr_number)?;

    if !input.skip_agents {
        // Non-headless: open each agent in a visible window (claude + tmux/zellij/macOS).
        let term = resolve_terminal(cfg.headless, &detected.client, "parley");

        if plan.spawn_mode == "team" {
            eprintln!("scrutiny parley: team lead…");
            run_team_parley(&detected, &plan, &comments, &cwd, term)?;
        } else {
            eprintln!("scrutiny parley: isolated members…");
            run_isolated_parley(&detected, &plan, &comments, &cwd, term)?;
        }

        // Verifier pass — both spawn modes, after fixes, before evangelist.
        if plan.verifiers > 0 {
            eprintln!(
                "scrutiny parley: {} verifier(s) check fixes…",
                plan.verifiers
            );
            run_verifier_parley(&detected, &plan, &comments, &cwd, term)?;
        }

        // Evangelist verify pass — isolated only
        if plan.evangelists > 0 && plan.spawn_mode == "isolated" {
            eprintln!(
                "scrutiny parley: {} evangelist(s) verify…",
                plan.evangelists
            );
            run_evangelist_parley(&detected, &plan, &comments, &cwd, term)?;
        }
    }

    let expected: Vec<String> = comments.comments.iter().map(|c| c.id.clone()).collect();
    let mut fixes = load_fixes(&fixes_path)?;
    // Seed missing from filesystem if agent wrote partial — validate will catch
    if let Err(e) = validate_fixes_complete(&fixes, &expected) {
        if input.skip_agents {
            // Seed stubs so ship/reply can still run in tests
            for id in &expected {
                if !fixes.fixes.iter().any(|f| &f.comment_id == id) {
                    fixes.fixes.push(FixEntry {
                        comment_id: id.clone(),
                        addressed: false,
                        reply_body: "Skipped (no agent)".into(),
                        explanation: "skip_agents".into(),
                        ..Default::default()
                    });
                }
            }
            save_fixes(&fixes_path, &fixes)?;
        } else {
            return Err(e);
        }
    }

    if !input.skip_ship {
        run_parley_ship(ParleyShipInput {
            cwd: &cwd,
            session_root: plan_path.parent().unwrap_or(&cwd),
            skip_prompts: input.non_interactive,
            client: &detected,
            model: &plan.model,
            push_fix_max_loops: cfg.parley.push_fix_max_loops,
        })?;
        eprintln!("scrutiny parley: post thread replies…");
        let (result, reply_path) = run_parley_reply(ParleyReplyInput {
            fixes_path: fixes_path.clone(),
            cwd: cwd.clone(),
        })?;
        eprintln!(
            "scrutiny parley: posted {} reply(ies) → {}",
            result.posted,
            reply_path.display()
        );

        let summary = ParleySessionSummary {
            version: 1,
            comments_path: comments_path.display().to_string(),
            plan_path: plan_path.display().to_string(),
            fixes_path: fixes_path.display().to_string(),
            reply_path: Some(reply_path.display().to_string()),
            comment_count: plan.comment_count,
            members: plan.members,
            verifiers: plan.verifiers,
            evangelists: plan.evangelists,
            spawn_mode: plan.spawn_mode.clone(),
        };
        let path = artifact_path("parley-session");
        write_json_pretty(&path, &summary)?;
        return Ok(path);
    }

    let summary = ParleySessionSummary {
        version: 1,
        comments_path: comments_path.display().to_string(),
        plan_path: plan_path.display().to_string(),
        fixes_path: fixes_path.display().to_string(),
        reply_path: None,
        comment_count: plan.comment_count,
        members: plan.members,
        verifiers: plan.verifiers,
        evangelists: plan.evangelists,
        spawn_mode: plan.spawn_mode.clone(),
    };
    let path = artifact_path("parley-session");
    write_json_pretty(&path, &summary)?;
    Ok(path)
}

/// Wall clock for a non-headless agent window (user may be watching — be generous).
const NONHEADLESS_WALL_SECS: u64 = AGENT_WALL_SECS * 3;

/// Read the fixes file and ensure every expected thread id has an entry,
/// stubbing any the agent left behind. Used to collect non-headless results.
fn collect_disk_fixes(fixes_path: &str, expected: &[String], note: &str) -> Result<()> {
    let mut file = load_fixes(Path::new(fixes_path))?;
    for id in expected {
        if !file.fixes.iter().any(|f| &f.comment_id == id) {
            file.fixes.push(FixEntry {
                comment_id: id.clone(),
                addressed: false,
                reply_body: note.to_string(),
                explanation: "no fix entry".into(),
                ..Default::default()
            });
        }
    }
    save_fixes(Path::new(fixes_path), &file)
}

fn run_isolated_parley(
    client: &crate::runtime::DetectedClient,
    plan: &ParleyPlan,
    comments: &ParleyCommentsFile,
    cwd: &Path,
    term: Option<TerminalContext>,
) -> Result<()> {
    if plan.buckets.is_empty() {
        bail!("parley isolated: no buckets");
    }

    if let Some(ctx) = term {
        let mut sentinels = Vec::new();
        for (i, bucket) in plan.buckets.iter().enumerate() {
            let index = (i + 1) as u32;
            let label = format!("parley-member#{index}");
            let slice = comments_for_ids(comments, bucket);
            let prompt =
                build_member_prompt(&plan.comments_path, &plan.fixes_path, &slice, index, false);
            sentinels.push(run_nonheadless(client, &plan.model, cwd, &prompt, &label, ctx)?);
        }
        let missing = wait_for_sentinels(&sentinels, Duration::from_secs(NONHEADLESS_WALL_SECS));
        if !missing.is_empty() {
            eprintln!(
                "scrutiny parley: {} member window(s) did not signal done within {}s — collecting partial fixes",
                missing.len(),
                NONHEADLESS_WALL_SECS
            );
        }
        let expected: Vec<String> = comments.comments.iter().map(|c| c.id.clone()).collect();
        return collect_disk_fixes(
            &plan.fixes_path,
            &expected,
            "Agent window finished without a fix entry.",
        );
    }

    let wall = Duration::from_secs(AGENT_WALL_SECS);
    let (tx, rx) = mpsc::channel();
    let job_count = plan.buckets.len();
    if job_count == 0 {
        bail!("parley isolated: no buckets");
    }

    for (i, bucket) in plan.buckets.iter().enumerate() {
        let tx = tx.clone();
        let client = client.clone();
        let model = plan.model.clone();
        let cwd = cwd.to_path_buf();
        let comments_path = plan.comments_path.clone();
        let fixes_path = plan.fixes_path.clone();
        let ids = bucket.clone();
        let slice = comments_for_ids(comments, &ids);
        let index = (i + 1) as u32;
        let label = format!("parley-member#{index}");
        thread::spawn(move || {
            let prompt = build_member_prompt(&comments_path, &fixes_path, &slice, index, false);
            let out = run_headless(
                &client,
                &model,
                &cwd,
                &prompt,
                HeadlessKind::Parley,
                &label,
                wall,
            );
            let _ = tx.send((index, ids, out));
        });
    }
    drop(tx);

    let mut collected: Vec<(u32, Vec<String>, Result<crate::agent_runner::HeadlessOutcome>)> =
        Vec::new();
    for _ in 0..job_count {
        collected.push(rx.recv().context("parley member channel")?);
    }
    collected.sort_by_key(|(i, _, _)| *i);

    let mut file = load_fixes(Path::new(&plan.fixes_path))?;
    for (index, ids, out) in collected {
        match out {
            Ok(o) => {
                let mut entries = parse_fixes_from_agent_stdout(&o.stdout);
                if entries.is_empty() {
                    // Agent may have written fixes file directly
                    let disk = load_fixes(Path::new(&plan.fixes_path))?;
                    for id in &ids {
                        if let Some(e) = disk.fixes.iter().find(|f| &f.comment_id == id) {
                            entries.push(e.clone());
                        }
                    }
                }
                // Reload file before merge in case peers wrote
                file = load_fixes(Path::new(&plan.fixes_path))?;
                merge_fix_entries(&mut file, &entries);
                // Ensure every assigned id has something
                for id in &ids {
                    if !file.fixes.iter().any(|f| &f.comment_id == id) {
                        file.fixes.push(FixEntry {
                            comment_id: id.clone(),
                            addressed: false,
                            reply_body: format!(
                                "Member #{index} finished without a structured fix entry."
                            ),
                            explanation: "agent omitted fix JSON".into(),
                            ..Default::default()
                        });
                    }
                }
                save_fixes(Path::new(&plan.fixes_path), &file)?;
                if o.code != 0 {
                    eprintln!(
                        "scrutiny parley: member#{index} exit {} — using available fixes",
                        o.code
                    );
                }
            }
            Err(e) => {
                eprintln!("scrutiny parley: member#{index} failed: {e:#}");
                for id in &ids {
                    if !file.fixes.iter().any(|f| &f.comment_id == id) {
                        file.fixes.push(FixEntry {
                            comment_id: id.clone(),
                            addressed: false,
                            reply_body: format!("Agent member#{index} failed: {e}"),
                            explanation: "agent error".into(),
                            ..Default::default()
                        });
                    }
                }
                save_fixes(Path::new(&plan.fixes_path), &file)?;
            }
        }
    }
    Ok(())
}

fn run_team_parley(
    client: &crate::runtime::DetectedClient,
    plan: &ParleyPlan,
    comments: &ParleyCommentsFile,
    cwd: &Path,
    term: Option<TerminalContext>,
) -> Result<()> {
    let wall = Duration::from_secs(AGENT_WALL_SECS);
    let prompt = build_team_lead_parley_prompt(plan, comments);

    if let Some(ctx) = term {
        let sentinel = run_nonheadless(client, &plan.model, cwd, &prompt, "parley-lead", ctx)?;
        let missing =
            wait_for_sentinels(&[sentinel], Duration::from_secs(NONHEADLESS_WALL_SECS));
        if !missing.is_empty() {
            eprintln!(
                "scrutiny parley: team lead window did not signal done within {}s — collecting partial fixes",
                NONHEADLESS_WALL_SECS
            );
        }
        let expected: Vec<String> = comments.comments.iter().map(|c| c.id.clone()).collect();
        return collect_disk_fixes(
            &plan.fixes_path,
            &expected,
            "Team lead window finished without a fix entry.",
        );
    }

    let out = run_headless(
        client,
        &plan.model,
        cwd,
        &prompt,
        HeadlessKind::Parley,
        "parley-lead",
        wall,
    )?;
    let mut file = load_fixes(Path::new(&plan.fixes_path))?;
    let entries = parse_fixes_from_agent_stdout(&out.stdout);
    if !entries.is_empty() {
        merge_fix_entries(&mut file, &entries);
    } else {
        // Prefer disk write from agent
        file = load_fixes(Path::new(&plan.fixes_path))?;
    }
    let expected: Vec<String> = comments.comments.iter().map(|c| c.id.clone()).collect();
    for id in &expected {
        if !file.fixes.iter().any(|f| &f.comment_id == id) {
            file.fixes.push(FixEntry {
                comment_id: id.clone(),
                addressed: false,
                reply_body: "Team lead finished without a structured fix entry.".into(),
                explanation: "lead omitted fix".into(),
                ..Default::default()
            });
        }
    }
    save_fixes(Path::new(&plan.fixes_path), &file)?;
    if out.code != 0 {
        eprintln!(
            "scrutiny parley: lead exit {} — using available fixes",
            out.code
        );
    }
    Ok(())
}

fn run_verifier_parley(
    client: &crate::runtime::DetectedClient,
    plan: &ParleyPlan,
    comments: &ParleyCommentsFile,
    cwd: &Path,
    term: Option<TerminalContext>,
) -> Result<()> {
    let prompt = build_verifier_prompt(plan, comments);
    run_verify_agents(client, plan, cwd, term, plan.verifiers, "parley-verifier", &prompt)
}

fn run_evangelist_parley(
    client: &crate::runtime::DetectedClient,
    plan: &ParleyPlan,
    comments: &ParleyCommentsFile,
    cwd: &Path,
    term: Option<TerminalContext>,
) -> Result<()> {
    let prompt = build_evangelist_prompt(plan, comments);
    run_verify_agents(
        client,
        plan,
        cwd,
        term,
        plan.evangelists,
        "parley-evangelist",
        &prompt,
    )
}

/// Run `count` verify-style agents (verifier / evangelist) that read comments +
/// fixes and amend the fixes file. Headless captures stdout; non-headless opens
/// one window per agent and waits on completion sentinels.
fn run_verify_agents(
    client: &crate::runtime::DetectedClient,
    plan: &ParleyPlan,
    cwd: &Path,
    term: Option<TerminalContext>,
    count: u32,
    label_prefix: &str,
    prompt: &str,
) -> Result<()> {
    if let Some(ctx) = term {
        let mut sentinels = Vec::new();
        for i in 0..count {
            let label = format!("{label_prefix}#{}", i + 1);
            sentinels.push(run_nonheadless(client, &plan.model, cwd, prompt, &label, ctx)?);
        }
        let missing = wait_for_sentinels(&sentinels, Duration::from_secs(NONHEADLESS_WALL_SECS));
        if !missing.is_empty() {
            eprintln!(
                "scrutiny parley: {} {label_prefix} window(s) did not signal done within {}s",
                missing.len(),
                NONHEADLESS_WALL_SECS
            );
        }
        return Ok(());
    }

    let wall = Duration::from_secs(AGENT_WALL_SECS);
    for i in 0..count {
        let label = format!("{label_prefix}#{}", i + 1);
        let out = run_headless(client, &plan.model, cwd, prompt, HeadlessKind::Parley, &label, wall)?;
        let mut file = load_fixes(Path::new(&plan.fixes_path))?;
        let entries = parse_fixes_from_agent_stdout(&out.stdout);
        if !entries.is_empty() {
            merge_fix_entries(&mut file, &entries);
            save_fixes(Path::new(&plan.fixes_path), &file)?;
        }
        if out.code != 0 {
            eprintln!("scrutiny parley: {label} exit {}", out.code);
        }
    }
    Ok(())
}

fn comments_for_ids(file: &ParleyCommentsFile, ids: &[String]) -> Vec<ParleyComment> {
    ids.iter()
        .filter_map(|id| file.comments.iter().find(|c| &c.id == id).cloned())
        .collect()
}

fn build_member_prompt(
    comments_path: &str,
    fixes_path: &str,
    slice: &[ParleyComment],
    index: u32,
    team_verify: bool,
) -> String {
    let mut p = String::new();
    p.push_str(&format!(
        "You are parley member #{index}. Address ONLY the PR review comments listed below.\n\
         Do NOT git commit, git push, or call gh to reply — the host script does that.\n\
         Do NOT touch comments outside your assignment.\n\n"
    ));
    if team_verify {
        p.push_str(
            "After fixing, do a clean-code / consistency pass on touched files before writing fixes.\n\n",
        );
    }
    p.push_str(&format!("Comments file (full): {comments_path}\n"));
    p.push_str(&format!("Fixes file to update: {fixes_path}\n\n"));
    p.push_str("## Assigned threads\n");
    for c in slice {
        let line = c
            .line
            .map(|l| l.to_string())
            .unwrap_or_else(|| "?".into());
        p.push_str(&format!(
            "### Thread `{}` — {}:{} (@{})\n{}\n\n",
            c.id, c.path, line, c.author, c.body
        ));
    }
    p.push_str(FIXES_PROTOCOL);
    p
}

fn build_team_lead_parley_prompt(plan: &ParleyPlan, comments: &ParleyCommentsFile) -> String {
    let mut p = String::new();
    p.push_str(
        "You are the parley team lead. Spawn/coordinate member agents for each bucket below \
         (or do the work yourself if you cannot spawn). Cover EVERY thread.\n\
         Do NOT git commit, git push, or call gh to reply — the host script does that.\n\n",
    );
    if plan.evangelists > 0 {
        p.push_str(&format!(
            "Also perform {} evangelist-style verify pass(es) on touches (architecture/consistency) before finishing.\n\n",
            plan.evangelists
        ));
    }
    p.push_str(&format!("Comments: {}\n", plan.comments_path));
    p.push_str(&format!("Fixes file: {}\n\n", plan.fixes_path));
    p.push_str("## Member buckets (thread ids)\n");
    for (i, bucket) in plan.buckets.iter().enumerate() {
        p.push_str(&format!("### Member {}\n", i + 1));
        for id in bucket {
            if let Some(c) = comments.comments.iter().find(|c| &c.id == id) {
                let line = c
                    .line
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "?".into());
                p.push_str(&format!(
                    "- `{}` {}:{} — {}\n",
                    c.id,
                    c.path,
                    line,
                    truncate(&c.body, 120)
                ));
            } else {
                p.push_str(&format!("- `{id}`\n"));
            }
        }
        p.push('\n');
    }
    p.push_str(FIXES_PROTOCOL);
    p
}

fn build_verifier_prompt(plan: &ParleyPlan, _comments: &ParleyCommentsFile) -> String {
    format!(
        "You are a parley verifier. Verify — do NOT re-implement — every entry in the fixes file.\n\
         Read:\n- comments: {}\n- fixes: {}\n\n\
         For EACH fix entry:\n\
         - If `addressed` is true: read the referenced code and confirm the change actually \
         resolves that review comment. If it truly does, set `verified: true`. If it does NOT \
         (missing, partial, wrong, or unrelated), set `verified: false`, set `addressed: false`, \
         and explain the gap in `verification` (and adjust `reply_body` to be honest).\n\
         - If `addressed` is false: confirm `reply_body` is a consistent, reasonable response to \
         the comment. Set `verified: true` if consistent, else `verified: false` and note why in \
         `verification`.\n\n\
         Do NOT edit source code. Do NOT git commit, push, or gh-reply. Only read-merge-write the \
         fixes JSON: preserve every existing field, add `verified` and `verification`, and flip \
         `addressed` only when a claimed fix does not hold.\n\n{}",
        plan.comments_path, plan.fixes_path, FIXES_PROTOCOL
    )
}

fn build_evangelist_prompt(plan: &ParleyPlan, _comments: &ParleyCommentsFile) -> String {
    format!(
        "You are a parley evangelist. Verify that PR review comment fixes are sound \
         (architecture, consistency, no half-fixed threads).\n\
         Read:\n- comments: {}\n- fixes: {}\n\
         You may edit code and amend `parley-fixes.json` entries (reply_body / explanation / snippets).\n\
         Do NOT git commit, push, or gh-reply.\n\n{}",
        plan.comments_path, plan.fixes_path, FIXES_PROTOCOL
    )
}

const FIXES_PROTOCOL: &str = "\
## Required output

Update the fixes JSON file (read-merge-write; keep other members' entries). For EACH assigned thread id, ensure an object with:

```json
{
  \"comment_id\": \"<thread id PRRT_…>\",
  \"addressed\": true,
  \"reply_body\": \"What to post under that GitHub thread\",
  \"explanation\": \"Why / what changed\",
  \"code_snippet\": \"optional key snippet\",
  \"files_touched\": [\"path\"]
}
```

Also print a final JSON block with a top-level `\"fixes\": [ … ]` array covering your assigned ids \
(so the host can merge if the file write is missed).\n\
If you truly will not fix a comment, set `addressed: false` and explain in reply_body.\n";

fn truncate(s: &str, max: usize) -> String {
    let t = s.replace('\n', " ");
    if t.chars().count() <= max {
        t
    } else {
        let cut: String = t.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

struct ParleyShipInput<'a> {
    cwd: &'a Path,
    session_root: &'a Path,
    skip_prompts: bool,
    client: &'a crate::runtime::DetectedClient,
    model: &'a str,
    push_fix_max_loops: u32,
}

struct PushAttempt {
    ok: bool,
    exit_code: i32,
    log_path: PathBuf,
    /// Full combined stdout+stderr (also written to log_path).
    log: String,
}

fn commit_msg_path(session_root: &Path) -> PathBuf {
    session_root.join("commit-msg.txt")
}

fn host_commit(cwd: &Path, session_root: &Path, subject: &str) -> Result<()> {
    let status = git_stdout(cwd, &["status", "--porcelain"])?;
    if status.trim().is_empty() {
        return Ok(());
    }
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
    let msg = format!("{subject}\n");
    let msg_path = commit_msg_path(session_root);
    fs::write(&msg_path, &msg).with_context(|| format!("write {}", msg_path.display()))?;
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
    eprintln!("scrutiny parley: committed — {subject}");
    Ok(())
}

/// True when push failed for auth/remote reasons an agent cannot fix.
pub fn is_non_fixable_push_error(log: &str) -> bool {
    let lower = log.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "authentication failed",
        "auth failed",
        "could not read username",
        "permission denied",
        "repository not found",
        "could not read from remote repository",
        "the requested url returned error: 403",
        "the requested url returned error: 401",
        "access denied",
        "invalid credentials",
        "fatal: could not read",
        "remote: permission to",
        "remote: write access",
        "error: failed to push some refs", // only alone is ambiguous — keep with auth-ish nearby
    ];
    // Broad auth/remote signals
    for n in NEEDLES {
        if lower.contains(n) {
            // "failed to push some refs" alone often means hook — only treat as
            // non-fixable when paired with auth/permission language nearby.
            if *n == "error: failed to push some refs" {
                let authish = lower.contains("denied")
                    || lower.contains("authentication")
                    || lower.contains("403")
                    || lower.contains("401")
                    || lower.contains("permission");
                // Hook/tests usually include husky / Failed Tests — still fixable
                // even if this line appears.
                let hookish = lower.contains("husky")
                    || lower.contains("failed tests")
                    || lower.contains("pre-push")
                    || lower.contains("pre-commit");
                if authish && !hookish {
                    return true;
                }
                continue;
            }
            // "permission denied" from husky script exit is rare; SSH/git auth common.
            // If husky/tests present, prefer fixable.
            if lower.contains("husky")
                || lower.contains("failed tests")
                || lower.contains("assertionerror")
                || lower.contains("pre-push script failed")
            {
                return false;
            }
            return true;
        }
    }
    false
}

fn push_log_tail(log: &str, max_chars: usize) -> String {
    let t = log.trim();
    if t.len() <= max_chars {
        return t.to_string();
    }
    let start = t.len().saturating_sub(max_chars);
    // Prefer char boundary
    let mut idx = start;
    while idx < t.len() && !t.is_char_boundary(idx) {
        idx += 1;
    }
    format!("…\n{}", &t[idx..])
}

fn run_parley_ship(input: ParleyShipInput<'_>) -> Result<()> {
    let cwd = input.cwd;
    let session_root = input.session_root;
    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let subject_default = "fix: address PR review comments".to_string();
    let commit_subject = if input.skip_prompts || !tty {
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
        eprintln!("scrutiny parley: working tree clean — skip commit");
    } else {
        host_commit(cwd, session_root, &commit_subject)?;
    }

    let branch = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".into())
        .trim()
        .to_string();
    let upstream = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "@{upstream}"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let push_args: Vec<&str> = if upstream.is_some() {
        vec!["push"]
    } else {
        vec!["push", "-u", "origin", "HEAD"]
    };
    let max_fix = input.push_fix_max_loops;
    // Total push attempts = 1 initial + max_fix retries after agent
    let max_push_attempts = 1 + max_fix;

    for attempt in 1..=max_push_attempts {
        let dest = match upstream {
            Some(ref up) => up.clone(),
            None => "origin (git push -u)".to_string(),
        };
        let sp = crate::spinner::Spinner::start(format!(
            "git push {branch} → {dest} — running pre-push hooks (attempt {attempt}/{max_push_attempts})"
        ));

        let log_path = session_root.join(format!("push-attempt-{attempt}.log"));
        let result = run_git_push_tee(cwd, &push_args, &log_path)?;
        if result.ok {
            sp.stop_ok("push complete");
            return Ok(());
        }
        sp.stop_fail(format!(
            "push failed (exit {}) — log {}",
            result.exit_code,
            result.log_path.display()
        ));

        // Log is on disk; show a short tail so the failure is visible at a glance.
        eprintln!(
            "scrutiny parley: push log (tail)\n{}",
            push_log_tail(&result.log, 2_000)
        );

        if is_non_fixable_push_error(&result.log) {
            bail!(
                "git push failed with auth/remote error (exit {}) — not spawning fix agent. See {}",
                result.exit_code,
                result.log_path.display()
            );
        }

        // No more fix cycles after this push attempt
        if attempt >= max_push_attempts {
            bail!(
                "git push failed (exit {}) after {max_push_attempts} attempt(s) — see {}",
                result.exit_code,
                result.log_path.display()
            );
        }

        let fix_n = attempt; // 1-based fix cycle matching failed push attempt
        eprintln!(
            "scrutiny parley: push failed — spawning fix agent (attempt {fix_n}/{max_fix})…"
        );
        let prompt = build_push_fix_prompt(&result.log_path, &result.log);
        let out = run_headless(
            input.client,
            input.model,
            cwd,
            &prompt,
            HeadlessKind::Parley,
            "parley-push-fix",
            Duration::from_secs(AGENT_WALL_SECS.saturating_mul(2)),
        )?;
        if out.code != 0 && !out.timed_out {
            eprintln!(
                "scrutiny parley: fix agent exit {} — checking for changes",
                out.code
            );
        }

        let dirty = git_stdout(cwd, &["status", "--porcelain"])?;
        if dirty.trim().is_empty() {
            eprintln!("scrutiny parley: fix agent left tree clean — retrying push anyway");
        } else {
            host_commit(cwd, session_root, "fix: repair pre-push failures")?;
        }
    }

    unreachable!("loop returns on success or bails")
}

fn build_push_fix_prompt(log_path: &Path, log: &str) -> String {
    let tail = push_log_tail(log, 12_000);
    format!(
        "git push failed (likely a pre-push hook: tests, lint, or typecheck).\n\
         Fix ONLY what blocks the push. Do NOT weaken, skip, or delete tests.\n\
         Do NOT git commit, git push, or call gh — the host script will commit and retry push.\n\n\
         Full push log on disk:\n- {}\n\n\
         ### Push log (tail)\n```\n{}\n```\n",
        log_path.display(),
        tail
    )
}

/// Run `git <args>`, stream stdout+stderr to the terminal, write combined log to `log_path`.
fn run_git_push_tee(cwd: &Path, args: &[&str], log_path: &Path) -> Result<PushAttempt> {
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }

    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn git {}", args.join(" ")))?;

    let stdout = child.stdout.take().context("git stdout")?;
    let stderr = child.stderr.take().context("git stderr")?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    let tx1 = tx.clone();
    let h1 = thread::spawn(move || drain_to_log(stdout, tx1));
    let h2 = thread::spawn(move || drain_to_log(stderr, tx));

    let mut combined = Vec::new();
    for chunk in rx {
        combined.extend_from_slice(&chunk);
    }
    let _ = h1.join();
    let _ = h2.join();

    let status = child.wait().context("wait git push")?;
    let log = String::from_utf8_lossy(&combined).into_owned();
    fs::write(log_path, &log).with_context(|| format!("write {}", log_path.display()))?;

    Ok(PushAttempt {
        ok: status.success(),
        exit_code: status.code().unwrap_or(-1),
        log_path: log_path.to_path_buf(),
        log,
    })
}

/// Drain a child pipe into the log channel only — no live echo to the terminal.
/// A spinner covers the wait; the full output lands in the on-disk push log.
fn drain_to_log(mut r: impl Read, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Discrete: write plan from existing comments JSON + answers.
pub fn run_parley_plan_from_paths(
    comments_path: &Path,
    answers: crate::parley::plan::ParleyAnswers,
    cfg: &Config,
) -> Result<(ParleyPlan, PathBuf)> {
    let comments = load_parley_comments(comments_path)?;
    run_parley_plan_write(ParleyPlanWriteInput {
        comments,
        comments_path: comments_path.to_path_buf(),
        answers,
        cfg: cfg.clone(),
    })
}

pub fn run_parley_plan_path(path: &Path) -> Result<ParleyPlan> {
    load_parley_plan(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_protocol_mentions_thread_id() {
        assert!(FIXES_PROTOCOL.contains("PRRT_"));
        assert!(FIXES_PROTOCOL.contains("comment_id"));
    }

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hi", 10), "hi");
        assert!(truncate("abcdefghijKLMN", 8).ends_with('…'));
    }

    #[test]
    fn push_auth_errors_non_fixable() {
        assert!(is_non_fixable_push_error(
            "fatal: Authentication failed for 'https://github.com/org/repo.git'"
        ));
        assert!(is_non_fixable_push_error(
            "ERROR: Repository not found.\nfatal: Could not read from remote repository."
        ));
        assert!(is_non_fixable_push_error(
            "Permission denied (publickey).\nfatal: Could not read from remote repository."
        ));
        assert!(is_non_fixable_push_error(
            "remote: Permission to org/repo.git denied to user.\nfatal: unable to access"
        ));
    }

    #[test]
    fn push_hook_test_failures_are_fixable() {
        let husky = r#"
 FAIL  src/hooks/useCustomerSearch.specs.tsx
AssertionError: expected 3rd "spy" call
❌ Failed checks: tests
husky - pre-push script failed (code 1)
error: failed to push some refs to 'github.com:tablecheck/manager-ember-desktop.git'
"#;
        assert!(!is_non_fixable_push_error(husky));
        assert!(!is_non_fixable_push_error(
            "pre-push script failed\nFAILED tests\nerror: failed to push some refs"
        ));
    }

    #[test]
    fn push_log_tail_keeps_end() {
        let s = "a".repeat(100);
        let t = push_log_tail(&s, 20);
        assert!(t.starts_with('…'));
        assert!(t.ends_with('a'));
        assert!(t.len() < 40);
    }
}
