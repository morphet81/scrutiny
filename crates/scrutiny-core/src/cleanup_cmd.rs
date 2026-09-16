//! `scrutiny cleanup` — tear down the current forge task tab / worktree / branch.

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::git::{
    self, delete_branch, is_linked_worktree, is_protected_branch, remove_worktree,
};
use crate::terminal::{
    close_current_mux_container, close_sibling_panes, kill_item_surface,
    load_item_surface_from_env, ItemSurface,
};

#[derive(Debug, Clone)]
pub struct CleanupCmdInput {
    pub cwd: PathBuf,
    pub non_interactive: bool,
}

/// Confirm (unless `-y`), then: sibling panes → worktree → branch → tab.
///
/// Git deletes are scoped to **this** checkout only:
/// - worktree path = `git rev-parse --show-toplevel` for cwd
/// - branch = HEAD of that worktree
/// - remove only if path is a registered *linked* worktree of the main repo
/// - never delete protected branches (`main` / `master` / …)
/// - delete branch only after worktree remove succeeds
pub fn run_cleanup(input: CleanupCmdInput) -> Result<()> {
    let cwd = input.cwd;
    git::ensure_git_repo(&cwd)?;

    let (worktree, main_root) = resolve_worktree_and_main(&cwd)?;
    let branch = git::git_stdout(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    let surface = load_task_surface(&worktree);
    let linked = is_linked_worktree(&main_root, &worktree);

    eprintln!("scrutiny cleanup:");
    eprintln!("  worktree: {}", worktree.display());
    if linked {
        eprintln!("  main:     {}", main_root.display());
    } else {
        eprintln!("  main:     (same checkout — will NOT remove worktree or branch)");
    }
    eprintln!("  branch:   {branch}");
    if is_protected_branch(&branch) {
        eprintln!("  note:     protected branch — will NOT delete it");
    }
    match &surface {
        Some(s) => eprintln!("  surface:  {}", surface_label(s)),
        None => eprintln!("  surface:  (none — will close current mux tab/window if any)"),
    }

    if !confirm_cleanup(input.non_interactive)? {
        eprintln!("scrutiny cleanup: aborted");
        return Ok(());
    }

    // 1) Close other panes in the current tab/window (no git deletes).
    match close_sibling_panes() {
        Ok(n) if n > 0 => eprintln!("scrutiny cleanup: closed {n} sibling pane(s)"),
        Ok(_) => eprintln!("scrutiny cleanup: no sibling panes to close"),
        Err(e) => eprintln!("scrutiny cleanup: skip sibling panes: {e:#}"),
    }

    // 2–3) Linked worktree only: remove THIS path, then ITS HEAD branch.
    if linked {
        if worktree_paths_equal(&worktree, &main_root) {
            bail!("internal: refusing to remove primary checkout as a worktree");
        }
        match remove_worktree(&main_root, &worktree) {
            Ok(()) => {
                eprintln!(
                    "scrutiny cleanup: removed worktree {}",
                    worktree.display()
                );
                if is_protected_branch(&branch) {
                    eprintln!(
                        "scrutiny cleanup: skip branch delete (protected: {branch})"
                    );
                } else {
                    match delete_branch(&main_root, &branch) {
                        Ok(()) => eprintln!("scrutiny cleanup: deleted branch {branch}"),
                        Err(e) => eprintln!("scrutiny cleanup: skip branch delete: {e:#}"),
                    }
                }
            }
            Err(e) => {
                eprintln!("scrutiny cleanup: skip worktree remove: {e:#}");
                eprintln!(
                    "scrutiny cleanup: skip branch delete (worktree still present)"
                );
            }
        }
    } else {
        eprintln!("scrutiny cleanup: skip worktree remove (not a linked worktree)");
        eprintln!("scrutiny cleanup: skip branch delete (not a linked worktree)");
    }

    // 4) Close the tab / session / window last (kills this shell if we are in it).
    eprintln!("scrutiny cleanup: closing tab…");
    let close_result = if let Some(ref s) = surface {
        kill_item_surface(s)
    } else {
        close_current_mux_container()
    };
    match close_result {
        Ok(()) => eprintln!("scrutiny cleanup: done"),
        Err(e) => eprintln!("scrutiny cleanup: skip tab close: {e:#}"),
    }
    Ok(())
}

fn confirm_cleanup(non_interactive: bool) -> Result<bool> {
    if non_interactive {
        eprintln!("scrutiny cleanup: --yes — proceeding");
        return Ok(true);
    }
    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if !tty {
        bail!("scrutiny cleanup needs a TTY to confirm (or pass -y / --yes)");
    }
    Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Close panes, delete this worktree + branch, and close the tab?")
        .default(false)
        .interact()
        .context("cleanup confirm")
}

/// `(this_toplevel, main_toplevel)`. Equal when cwd is the primary checkout.
fn resolve_worktree_and_main(cwd: &Path) -> Result<(PathBuf, PathBuf)> {
    let this = PathBuf::from(
        git::git_stdout(cwd, &["rev-parse", "--show-toplevel"])?
            .trim(),
    );
    let common = PathBuf::from(
        git::git_stdout(
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?
        .trim(),
    );
    // common-dir is `<main>/.git`.
    let main = common
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| this.clone());
    let this_c = this.canonicalize().unwrap_or_else(|_| this.clone());
    let main_c = main.canonicalize().unwrap_or_else(|_| main.clone());
    if this_c == main_c {
        Ok((this.clone(), this))
    } else {
        Ok((this, main))
    }
}

fn worktree_paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

fn load_task_surface(worktree: &Path) -> Option<ItemSurface> {
    if let Some(s) = load_item_surface_from_env() {
        return Some(s);
    }
    let scrutiny = worktree.join(".scrutiny");
    let entries = fs::read_dir(&scrutiny).ok()?;
    for ent in entries.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("forge") {
            continue;
        }
        let path = ent.path().join("item-surface.json");
        if !path.is_file() {
            continue;
        }
        let raw = fs::read_to_string(&path).ok()?;
        if let Ok(s) = serde_json::from_str::<ItemSurface>(&raw) {
            return Some(s);
        }
    }
    None
}

fn surface_label(s: &ItemSurface) -> String {
    match s {
        ItemSurface::Tmux { session } => format!("tmux session `{session}`"),
        ItemSurface::Zellij { tab, tab_id } => match tab_id {
            Some(id) => format!("zellij tab `{tab}` (id {id})"),
            None => format!("zellij tab `{tab}`"),
        },
        ItemSurface::ITerm2 { window_id } => format!("iTerm2 window {window_id}"),
        ItemSurface::Apple { window_id } => format!("Terminal.app window {window_id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{
        create_worktree, delete_branch, is_linked_worktree, is_protected_branch, remove_worktree,
    };
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
        let st = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    #[test]
    fn protected_branches() {
        assert!(is_protected_branch("main"));
        assert!(is_protected_branch("master"));
        assert!(!is_protected_branch("feat-nero-123"));
    }

    #[test]
    fn only_removes_registered_linked_worktree_and_its_branch() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-b", "main"]);
        git(&main, &["config", "user.email", "t@t"]);
        git(&main, &["config", "user.name", "t"]);
        fs::write(main.join("f"), "x").unwrap();
        git(&main, &["add", "f"]);
        git(&main, &["commit", "-m", "init"]);

        let wt = dir.path().join("feat-nero-1");
        create_worktree(&main, "feat-nero-1", &wt).unwrap();
        assert!(is_linked_worktree(&main, &wt));
        assert!(!is_linked_worktree(&main, &main));

        // Wrong path must not look linked.
        assert!(!is_linked_worktree(&main, &dir.path().join("nope")));

        remove_worktree(&main, &wt).unwrap();
        assert!(!wt.exists());
        delete_branch(&main, "feat-nero-1").unwrap();

        // main still intact
        assert!(main.join("f").is_file());
        assert!(
            delete_branch(&main, "main").is_err(),
            "must refuse deleting main"
        );
    }
}
