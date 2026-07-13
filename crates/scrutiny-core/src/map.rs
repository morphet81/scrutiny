use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::eval::EvalReport;
use crate::git;
use crate::paths::{temp_artifact_path, write_json_pretty};
use crate::score::Tier;
use crate::taxonomy::{is_risk_path, suggested_scope_for_tier};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapReport {
    pub version: u32,
    pub eval_path: String,
    pub repo: String,
    pub branch: String,
    pub base: String,
    pub head: String,
    pub tier: Tier,
    pub source_to_review: Vec<SourceEntry>,
    pub docs_semantic: Vec<DocEntry>,
    pub tests_related: Vec<TestEntry>,
    pub noise_skipped: Vec<NoiseEntry>,
    pub risk_tags: Vec<RiskTag>,
    pub suggested_scope: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEntry {
    pub path: String,
    pub change_kind: String,
    pub layer: Option<String>,
    pub hotspots: Vec<String>,
    pub focus: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocEntry {
    pub path: String,
    pub change_kind: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestEntry {
    pub path: String,
    pub change_kind: String,
    pub related_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoiseEntry {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskTag {
    pub tag: String,
    pub paths: Vec<String>,
    pub reason: String,
}

pub fn run_map(eval_path: &Path, cwd: &Path) -> Result<(MapReport, PathBuf)> {
    let text = fs::read_to_string(eval_path)
        .with_context(|| format!("read eval {}", eval_path.display()))?;
    let eval: EvalReport = serde_json::from_str(&text).context("parse eval json")?;

    let repo = git::discover_repo(cwd)?;
    let mut source_to_review = Vec::new();
    let mut docs_semantic = Vec::new();
    let mut tests_related = Vec::new();
    let mut security_paths = Vec::new();
    let mut perf_paths = Vec::new();
    let mut err_paths = Vec::new();

    for f in &eval.files {
        let change_kind = match f.status.as_str() {
            "A" => "add",
            "D" => "del",
            "R" => "rename",
            _ => "mod",
        }
        .to_string();

        match f.kind.as_str() {
            "doc" => {
                docs_semantic.push(DocEntry {
                    path: f.path.clone(),
                    change_kind,
                    note: "Needs semantic analysis (meaning, accuracy, drift vs code)".into(),
                });
            }
            "test" => {
                tests_related.push(TestEntry {
                    path: f.path.clone(),
                    change_kind,
                    related_hint: guess_related_source(&f.path),
                });
            }
            "source" | "config" | "other" => {
                let hotspots = extract_hotspots(&repo.root, &eval.base, &eval.head, &f.path)?;
                let focus = if f.risk {
                    "Risk path — prioritize correctness + security".into()
                } else {
                    format!(
                        "Review diff in {}; layer={:?}",
                        f.path,
                        f.layer.as_deref().unwrap_or("?")
                    )
                };
                if f.risk || is_risk_path(&f.path) {
                    security_paths.push(f.path.clone());
                }
                if looks_perf_path(&f.path) {
                    perf_paths.push(f.path.clone());
                }
                if looks_error_path(&f.path) || f.kind == "source" {
                    err_paths.push(f.path.clone());
                }
                source_to_review.push(SourceEntry {
                    path: f.path.clone(),
                    change_kind,
                    layer: f.layer.clone(),
                    hotspots,
                    focus,
                });
            }
            _ => {}
        }
    }

    let noise_skipped: Vec<NoiseEntry> = eval
        .excluded
        .iter()
        .map(|e| NoiseEntry {
            path: e.path.clone(),
            reason: e.reason.clone(),
        })
        .collect();

    let mut risk_tags = Vec::new();
    if !security_paths.is_empty() {
        risk_tags.push(RiskTag {
            tag: "security".into(),
            paths: security_paths,
            reason: "Path or content suggests auth/security/permissions surface".into(),
        });
    }
    if !perf_paths.is_empty() {
        risk_tags.push(RiskTag {
            tag: "performance".into(),
            paths: perf_paths,
            reason: "Hot path / list / loop / render intensive areas".into(),
        });
    }
    if !err_paths.is_empty() {
        risk_tags.push(RiskTag {
            tag: "error_handling".into(),
            paths: err_paths.into_iter().take(40).collect(),
            reason: "Source changes may need error / edge-case review".into(),
        });
    }

    let report = MapReport {
        version: 1,
        eval_path: eval_path.display().to_string(),
        repo: eval.repo.clone(),
        branch: eval.branch.clone(),
        base: eval.base.clone(),
        head: eval.head.clone(),
        tier: eval.tier,
        source_to_review,
        docs_semantic,
        tests_related,
        noise_skipped,
        risk_tags,
        suggested_scope: suggested_scope_for_tier(eval.tier),
    };

    let out = temp_artifact_path(&eval.repo, &eval.branch, "map");
    write_json_pretty(&out, &report)?;
    Ok((report, out))
}

fn guess_related_source(test_path: &str) -> String {
    test_path
        .replace(".specs.tsx", ".tsx")
        .replace(".specs.ts", ".ts")
        .replace(".test.tsx", ".tsx")
        .replace(".test.ts", ".ts")
        .replace(".spec.tsx", ".tsx")
        .replace(".spec.ts", ".ts")
}

fn looks_perf_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.contains("list")
        || p.contains("table")
        || p.contains("grid")
        || p.contains("virtual")
        || p.contains("cache")
        || p.contains("timetable")
        || p.contains("render")
}

fn looks_error_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.contains("error") || p.contains("catch") || p.contains("retry") || p.contains("toast")
}

fn extract_hotspots(root: &Path, base: &str, head: &str, path: &str) -> Result<Vec<String>> {
    let diff = git::diff_unified_paths(root, base, head, &[path.to_string()]).unwrap_or_default();
    let mut hits = Vec::new();
    for line in diff.lines() {
        // Added lines only
        if !line.starts_with('+') || line.starts_with("+++") {
            continue;
        }
        let body = &line[1..];
        if let Some(name) = extract_symbol(body) {
            if !hits.contains(&name) {
                hits.push(name);
            }
        }
        if hits.len() >= 12 {
            break;
        }
    }
    Ok(hits)
}

fn extract_symbol(line: &str) -> Option<String> {
    let trimmed = line.trim();
    // function foo(
    if let Some(rest) = trimmed.strip_prefix("function ") {
        let name = rest.split('(').next()?.trim();
        if !name.is_empty() {
            return Some(format!("fn:{name}"));
        }
    }
    // export function / export const
    if trimmed.starts_with("export function ") {
        let name = trimmed
            .trim_start_matches("export function ")
            .split('(')
            .next()?
            .trim();
        return Some(format!("fn:{name}"));
    }
    if trimmed.starts_with("export const ") {
        let name = trimmed
            .trim_start_matches("export const ")
            .split('=')
            .next()?
            .trim();
        return Some(format!("const:{name}"));
    }
    // fn foo( / pub fn foo(
    if let Some(idx) = trimmed.find("fn ") {
        let rest = &trimmed[idx + 3..];
        let name = rest.split('(').next()?.trim();
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Some(format!("fn:{name}"));
        }
    }
    // class Foo
    if let Some(rest) = trimmed.strip_prefix("class ") {
        let name = rest.split(|c: char| c == ' ' || c == '{' || c == '<').next()?.trim();
        if !name.is_empty() {
            return Some(format!("class:{name}"));
        }
    }
    None
}
