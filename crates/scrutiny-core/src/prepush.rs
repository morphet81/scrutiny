//! Quiet pre-push check gate shared by `parley` and `forge`.
//!
//! Scrutiny runs the repo's pre-push checks itself — quietly, capturing all
//! output to a dedicated log file rather than streaming it to the terminal (a
//! flood of hook output corrupts multiplexer panes; see `spinner.rs`). A fix
//! agent then works *only* from that log; it must not run the checks itself.
//! Scrutiny re-runs the checks to verify each fix.

use std::path::{Path, PathBuf};

use crate::git::git_stdout;

/// One line injected into implementing-agent briefs so each agent owns making
/// its own changes pass the repo's pre-push checks.
pub const PREPUSH_OWNERSHIP: &str =
    "Your changes must pass the repo's pre-push checks (lint / tests / typecheck). \
     Make every file you touch pass them before you finish.\n";

/// One line telling agents not to strew build/test artifacts across the repo.
/// scrutiny excludes known artifact globs from commits, but agents inventing new
/// output dirs defeats that — keep coverage in the repo's own gitignored dir.
pub const NO_ARTIFACTS: &str =
    "Do NOT run coverage/tests into custom output directories. Use the repo's own \
     scripts (e.g. `npm run test:coverage` writes to the gitignored `coverage/`). \
     Leave no build or coverage artifacts behind.\n";

/// Outcome of one quiet check run.
pub struct PrepushResult {
    pub ok: bool,
    pub exit_code: i32,
    pub log_path: PathBuf,
}

/// Resolve the shell command that runs the repo's pre-push checks.
///
/// - `override_cmd` (config) wins when non-empty.
/// - Otherwise, if a pre-push hook exists, run it the way git would
///   (`git hook run pre-push`, which honors husky's `core.hooksPath`).
/// - Otherwise `None` — no checks to run; the gate is a no-op green.
pub fn resolve_prepush_command(cwd: &Path, override_cmd: Option<&str>) -> Option<String> {
    if let Some(c) = override_cmd {
        let c = c.trim();
        if !c.is_empty() {
            return Some(c.to_string());
        }
    }
    if prepush_hook_exists(cwd) {
        return Some("git hook run pre-push".to_string());
    }
    None
}

/// True when the repo has an executable-ish pre-push hook (honors `core.hooksPath`).
fn prepush_hook_exists(cwd: &Path) -> bool {
    let hooks_dir = match git_stdout(cwd, &["config", "--get", "core.hooksPath"]) {
        Ok(s) if !s.trim().is_empty() => PathBuf::from(s.trim()),
        _ => match git_stdout(cwd, &["rev-parse", "--git-path", "hooks"]) {
            Ok(s) if !s.trim().is_empty() => PathBuf::from(s.trim()),
            _ => return false,
        },
    };
    let hook = if hooks_dir.is_absolute() {
        hooks_dir.join("pre-push")
    } else {
        cwd.join(hooks_dir).join("pre-push")
    };
    hook.exists()
}

/// Run `cmd` quietly (no terminal echo), writing combined stdout+stderr to
/// `log_path`. A spinner covers the wait at the call site.
pub fn run_checks_to_log(cwd: &Path, cmd: &str, log_path: &Path) -> std::io::Result<PrepushResult> {
    let (code, out, err) = crate::forge::verify::run_command(cwd, cmd);
    let combined = format!(
        "$ {cmd}\n\n----- stdout -----\n{out}\n----- stderr -----\n{err}\n----- exit {code} -----\n"
    );
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(log_path, combined.as_bytes())?;
    Ok(PrepushResult {
        ok: code == 0,
        exit_code: code,
        log_path: log_path.to_path_buf(),
    })
}

/// Prompt for a fix agent: act only on scrutiny's logged findings; never run the
/// checks, commit, or push.
pub fn build_prepush_fix_prompt(findings_path: &Path) -> String {
    format!(
        "The repository's pre-push checks (lint / tests / typecheck) are FAILING. \
         scrutiny already ran them and saved the full output to disk.\n\n\
         Findings file (read it — this is your only input): {}\n\n\
         Fix ONLY what the findings report as failing. Do NOT weaken, skip, or delete tests.\n\
         Do NOT run the checks, lint, tests, build, or any pre-push command yourself — \
         scrutiny re-runs them and verifies your fix.\n\
         Do NOT git commit, git push, or call gh — the host script commits and retries.\n",
        findings_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_over_hook() {
        // A non-empty override is returned verbatim regardless of hook presence.
        let cmd = resolve_prepush_command(Path::new("/nonexistent"), Some("npm run verify"));
        assert_eq!(cmd.as_deref(), Some("npm run verify"));
    }

    #[test]
    fn blank_override_is_ignored() {
        // Blank override + no git repo → no hook → None.
        let cmd = resolve_prepush_command(Path::new("/nonexistent"), Some("   "));
        assert!(cmd.is_none());
    }

    #[test]
    fn fix_prompt_forbids_running_checks() {
        let p = build_prepush_fix_prompt(Path::new("/tmp/prepush-check-1.log"));
        assert!(p.contains("Do NOT run the checks"));
        assert!(p.contains("/tmp/prepush-check-1.log"));
        assert!(p.contains("Do NOT git commit"));
    }
}
