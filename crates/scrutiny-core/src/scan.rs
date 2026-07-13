use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{ensure_config, find_shipped_default, load_config};
use crate::eval::EvalReport;
use crate::git;
use crate::map::MapReport;
use crate::pack::PackReport;
use crate::paths::{temp_artifact_path, write_json_pretty};
use crate::score::Tier;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanReport {
    pub version: u32,
    pub map_path: String,
    pub pack_path: Option<String>,
    pub eval_path: Option<String>,
    pub repo: String,
    pub branch: String,
    pub tier: Tier,
    pub architecture_risk: bool,
    pub findings: Vec<Finding>,
}

/// Caveman-ready finding shape (matches skill Step 5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub number: u32,
    pub title: String,
    pub explanation: String,
    pub proposed_fix: String,
    pub fix_options: Vec<String>,
    pub severity: String,
    pub source: String,
    pub paths: Vec<String>,
    pub bucket: String,
}

pub fn run_scan(
    map_path: &Path,
    pack_path: Option<&Path>,
    eval_path: Option<&Path>,
    cwd: &Path,
) -> Result<(ScanReport, PathBuf)> {
    let map: MapReport = serde_json::from_str(
        &fs::read_to_string(map_path)
            .with_context(|| format!("read map {}", map_path.display()))?,
    )
    .context("parse map json")?;

    let pack: Option<PackReport> = if let Some(p) = pack_path {
        Some(
            serde_json::from_str(
                &fs::read_to_string(p).with_context(|| format!("read pack {}", p.display()))?,
            )
            .context("parse pack json")?,
        )
    } else {
        None
    };

    let eval: Option<EvalReport> = if let Some(p) = eval_path {
        Some(
            serde_json::from_str(
                &fs::read_to_string(p).with_context(|| format!("read eval {}", p.display()))?,
            )
            .context("parse eval json")?,
        )
    } else if !map.eval_path.is_empty() {
        let p = Path::new(&map.eval_path);
        if p.exists() {
            Some(
                serde_json::from_str(&fs::read_to_string(p).context("read map.eval_path")?)
                    .context("parse eval from map")?,
            )
        } else {
            None
        }
    } else {
        None
    };

    let shipped = find_shipped_default(&std::env::current_exe().unwrap_or_else(|_| cwd.to_path_buf()));
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;

    let architecture_risk = pack
        .as_ref()
        .map(|p| p.architecture_risk)
        .unwrap_or_else(|| matches!(map.tier, Tier::L | Tier::Xl));

    if !cfg.scan.enable {
        let report = empty_report(map_path, pack_path, eval_path, &map, architecture_risk);
        let out = temp_artifact_path(&map.repo, &map.branch, "scan");
        write_json_pretty(&out, &report)?;
        return Ok((report, out));
    }

    let repo = git::discover_repo(cwd)?;
    let mut findings = Vec::new();

    collect_diff_findings(&repo.root, &map, &mut findings)?;
    collect_missing_tests(&map, eval.as_ref(), &mut findings);
    collect_risk_without_test(&map, &mut findings);
    collect_large_hunks(eval.as_ref(), pack.as_ref(), &mut findings);

    for cmd in &cfg.scan.commands {
        if let Some(f) = run_lint_hook(cwd, cmd) {
            findings.push(f);
        }
    }

    for (i, f) in findings.iter_mut().enumerate() {
        f.number = (i + 1) as u32;
    }

    let report = ScanReport {
        version: 1,
        map_path: map_path.display().to_string(),
        pack_path: pack_path.map(|p| p.display().to_string()),
        eval_path: eval.as_ref().map(|_| {
            eval_path
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| map.eval_path.clone())
        }),
        repo: map.repo.clone(),
        branch: map.branch.clone(),
        tier: map.tier,
        architecture_risk,
        findings,
    };

    let out = temp_artifact_path(&map.repo, &map.branch, "scan");
    write_json_pretty(&out, &report)?;
    Ok((report, out))
}

fn empty_report(
    map_path: &Path,
    pack_path: Option<&Path>,
    eval_path: Option<&Path>,
    map: &MapReport,
    architecture_risk: bool,
) -> ScanReport {
    ScanReport {
        version: 1,
        map_path: map_path.display().to_string(),
        pack_path: pack_path.map(|p| p.display().to_string()),
        eval_path: eval_path.map(|p| p.display().to_string()),
        repo: map.repo.clone(),
        branch: map.branch.clone(),
        tier: map.tier,
        architecture_risk,
        findings: Vec::new(),
    }
}

fn collect_diff_findings(root: &Path, map: &MapReport, out: &mut Vec<Finding>) -> Result<()> {
    let mut paths: Vec<String> = map
        .source_to_review
        .iter()
        .map(|s| s.path.clone())
        .chain(map.tests_related.iter().map(|t| t.path.clone()))
        .collect();
    paths.sort();
    paths.dedup();

    for path in paths {
        let diff =
            git::diff_unified_paths(root, &map.base, &map.head, &[path.clone()]).unwrap_or_default();
        for line in diff.lines() {
            if !line.starts_with('+') || line.starts_with("+++") {
                continue;
            }
            let body = &line[1..];
            let lower = body.to_ascii_lowercase();

            if lower.contains("todo") || lower.contains("fixme") || lower.contains("hack") {
                out.push(finding(
                    "TODO/FIXME/HACK in added lines",
                    format!("Added marker in `{path}`: {}", body.trim()),
                    "Resolve or track outside this change before merge.",
                    "medium",
                    "scan.diff_marker",
                    vec![path.clone()],
                    "Bugs",
                ));
            }
            if lower.contains("console.log")
                || lower.contains("debugger")
                || lower.contains("console.debug")
            {
                out.push(finding(
                    "Debug leftover in added lines",
                    format!("Possible debug call in `{path}`: {}", body.trim()),
                    "Remove before merge.",
                    "medium",
                    "scan.debug",
                    vec![path.clone()],
                    "Bugs",
                ));
            }
            if body.contains(".unwrap()")
                || body.contains(".expect(")
                || body.contains("panic!(")
                || body.contains("unreachable!(")
            {
                out.push(finding(
                    "Panic/unwrap in added lines",
                    format!("Hard fail in `{path}`: {}", body.trim()),
                    "Prefer Result/? or graceful error path.",
                    "medium",
                    "scan.unwrap",
                    vec![path.clone()],
                    "Bugs",
                ));
            }
        }
    }
    dedup_findings(out);
    Ok(())
}

fn collect_missing_tests(map: &MapReport, eval: Option<&EvalReport>, out: &mut Vec<Finding>) {
    let test_paths: Vec<&str> = map.tests_related.iter().map(|t| t.path.as_str()).collect();
    let source_changed: Vec<&str> = map
        .source_to_review
        .iter()
        .filter(|s| s.change_kind != "del")
        .map(|s| s.path.as_str())
        .collect();

    if source_changed.is_empty() || !test_paths.is_empty() {
        return;
    }
    let has_source = eval
        .map(|e| e.files.iter().any(|f| f.kind == "source"))
        .unwrap_or(true);
    if !has_source {
        return;
    }
    out.push(finding(
        "Source changed without companion test",
        format!(
            "{} source file(s) changed; no test files in map.",
            source_changed.len()
        ),
        "Add or update a companion test for the behavior change.",
        "medium",
        "scan.missing_test",
        source_changed.iter().take(8).map(|s| s.to_string()).collect(),
        "Test inconsistency",
    ));
}

fn collect_risk_without_test(map: &MapReport, out: &mut Vec<Finding>) {
    let has_tests = !map.tests_related.is_empty();
    let security = map.risk_tags.iter().find(|t| t.tag == "security");
    let Some(sec) = security else {
        return;
    };
    if has_tests {
        return;
    }
    out.push(finding(
        "Risk path touched without test",
        format!(
            "Security-tagged paths changed with no related tests: {}",
            sec.paths
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ),
        "Add regression coverage for auth/permissions/security surface.",
        "high",
        "scan.risk_no_test",
        sec.paths.clone(),
        "Critical",
    ));
}

fn collect_large_hunks(
    eval: Option<&EvalReport>,
    pack: Option<&PackReport>,
    out: &mut Vec<Finding>,
) {
    const LARGE_LOC: u32 = 200;
    if let Some(e) = eval {
        for f in &e.files {
            if f.kind != "source" {
                continue;
            }
            let loc = f.added + f.deleted;
            if loc >= LARGE_LOC {
                out.push(finding(
                    "Large file hunk",
                    format!("`{}` changes ~{loc} lines — hard to review.", f.path),
                    "Split change or extract smaller units before merge.",
                    "low",
                    "scan.large_hunk",
                    vec![f.path.clone()],
                    "Architecture & clean code",
                ));
            }
        }
    }
    if let Some(p) = pack {
        for s in &p.slices {
            for sym in &s.symbol_slices {
                let lines = sym.content.lines().count();
                if lines >= 120 {
                    out.push(finding(
                        "God-function sized symbol slice",
                        format!(
                            "`{}` slice ~{lines} lines — possible god function.",
                            sym.label
                        ),
                        "Extract helpers; keep reviewable unit size.",
                        "low",
                        "scan.god_fn",
                        vec![s.path.clone()],
                        "Architecture & clean code",
                    ));
                }
            }
        }
    }
}

fn run_lint_hook(cwd: &Path, cmd: &str) -> Option<Finding> {
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let flag = if cfg!(windows) { "/C" } else { "-c" };
    let out = Command::new(shell)
        .args([flag, cmd])
        .current_dir(cwd)
        .output()
        .ok()?;
    if out.status.success() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim().chars().take(400).collect::<String>()
    } else {
        stdout.trim().chars().take(400).collect::<String>()
    };
    Some(finding(
        "Project lint hook failed",
        format!("Command `{cmd}` exited non-zero. {detail}"),
        "Fix lint/tool output or adjust scan.commands in config.",
        "medium",
        "scan.lint_hook",
        Vec::new(),
        "Bugs",
    ))
}

fn finding(
    title: &str,
    explanation: impl Into<String>,
    proposed_fix: &str,
    severity: &str,
    source: &str,
    paths: Vec<String>,
    bucket: &str,
) -> Finding {
    Finding {
        number: 0,
        title: title.into(),
        explanation: explanation.into(),
        proposed_fix: proposed_fix.into(),
        fix_options: Vec::new(),
        severity: severity.into(),
        source: source.into(),
        paths,
        bucket: bucket.into(),
    }
}

fn dedup_findings(out: &mut Vec<Finding>) {
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|f| {
        let key = format!(
            "{}|{}|{}",
            f.title,
            f.paths.first().unwrap_or(&String::new()),
            f.source
        );
        seen.insert(key)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_shape_serializes() {
        let f = finding(
            "t",
            "e",
            "fix",
            "low",
            "scan.test",
            vec!["a.rs".into()],
            "Bugs",
        );
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["title"], "t");
        assert!(v["fix_options"].as_array().unwrap().is_empty());
    }
}
