use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::config::{ensure_config, find_shipped_default, load_config, Config, SuggestedPlan};
use crate::git::{self, DiffFile, RepoContext};
use crate::paths::{temp_artifact_path, write_json_pretty};
use crate::score::{compute_scatter, score_tier, ScoreSignals, Tier};
use crate::taxonomy::{
    blast_stub_for_path, change_class, classify_path, is_risk_path, layer_for_path, PathKind,
};

#[derive(Debug, Clone)]
pub struct EvalInput {
    pub cwd: PathBuf,
    /// Override head (default HEAD). For PR mode, PR head SHA or ref.
    pub head: Option<String>,
    /// Override base (e.g. PR base ref).
    pub base: Option<String>,
    pub client: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    pub version: u32,
    pub mode: String,
    pub repo: String,
    pub branch: String,
    pub base: String,
    pub head: String,
    pub tier: Tier,
    pub score: u32,
    pub signals: ScoreSignals,
    pub files: Vec<EvalFile>,
    pub excluded: Vec<ExcludedFile>,
    pub suggested_plan: SuggestedPlan,
    pub config_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalFile {
    pub path: String,
    pub status: String,
    pub added: u32,
    pub deleted: u32,
    pub kind: String,
    pub layer: Option<String>,
    pub risk: bool,
    pub blast_stub: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludedFile {
    pub path: String,
    pub reason: String,
    pub added: u32,
    pub deleted: u32,
}

pub fn run_eval(input: EvalInput) -> Result<(EvalReport, PathBuf)> {
    let repo = git::discover_repo(&input.cwd)?;
    let shipped = find_shipped_default(&std::env::current_exe().unwrap_or_else(|_| input.cwd.clone()));
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;

    let head = input
        .head
        .clone()
        .unwrap_or_else(|| "HEAD".to_string());
    let base = if let Some(b) = &input.base {
        b.clone()
    } else {
        git::resolve_base_branch(&repo.root, &cfg.git.base_candidates, None)?
    };

    let mode = if input.base.is_some() {
        "pr".to_string()
    } else {
        "local".to_string()
    };

    let globset = build_globset(&cfg.git.exclude_globs)?;
    let all = git::diff_numstat(&repo.root, &base, &head)?;

    let mut relevant = Vec::new();
    let mut excluded = Vec::new();
    for f in all {
        if globset.is_match(&f.path) {
            excluded.push(ExcludedFile {
                path: f.path,
                reason: "exclude_glob".into(),
                added: f.added,
                deleted: f.deleted,
            });
            continue;
        }
        relevant.push(f);
    }

    let report = build_report(
        &repo,
        &cfg,
        &cfg_path,
        &mode,
        &base,
        &head,
        relevant,
        excluded,
        input.client.as_deref(),
    )?;

    let out = temp_artifact_path(&repo.repo_slug, &repo.branch, "eval");
    write_json_pretty(&out, &report)?;
    Ok((report, out))
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let g = Glob::new(p).with_context(|| format!("bad glob: {p}"))?;
        b.add(g);
    }
    Ok(b.build()?)
}

fn build_report(
    repo: &RepoContext,
    cfg: &Config,
    cfg_path: &Path,
    mode: &str,
    base: &str,
    head: &str,
    relevant: Vec<DiffFile>,
    excluded: Vec<ExcludedFile>,
    client_override: Option<&str>,
) -> Result<EvalReport> {
    let mut kinds = Vec::new();
    let mut layers = BTreeSet::new();
    let mut risk_hits = 0u32;
    let mut blast = 0u32;
    let mut added = 0u32;
    let mut deleted = 0u32;
    let mut file_locs = Vec::new();
    let mut files = Vec::new();

    for f in &relevant {
        let kind = classify_path(&f.path);
        kinds.push(kind.clone());
        if let Some(layer) = layer_for_path(&f.path) {
            layers.insert(layer.to_string());
        }
        let risk = is_risk_path(&f.path);
        if risk {
            risk_hits += 1;
        }
        let b = blast_stub_for_path(&f.path);
        blast = blast.saturating_add(b);
        let loc = f.added + f.deleted;
        file_locs.push(loc);
        added += f.added;
        deleted += f.deleted;
        files.push(EvalFile {
            path: f.path.clone(),
            status: f.status.clone(),
            added: f.added,
            deleted: f.deleted,
            kind: kind_str(&kind).into(),
            layer: layer_for_path(&f.path).map(|s| s.to_string()),
            risk,
            blast_stub: b,
        });
    }

    let signals = ScoreSignals {
        relevant_files: relevant.len() as u32,
        relevant_loc: added + deleted,
        added,
        deleted,
        scatter: compute_scatter(&file_locs),
        blast_stub: blast,
        risk_path_hits: risk_hits,
        layers_touched: layers.into_iter().collect(),
        change_class: change_class(&kinds),
    };

    let (tier, score) = score_tier(&signals);
    let client = client_override
        .unwrap_or(cfg.default_client.as_str())
        .to_string();
    let suggested_plan = cfg.suggested_plan(&client, tier);

    Ok(EvalReport {
        version: 1,
        mode: mode.to_string(),
        repo: repo.repo_slug.clone(),
        branch: repo.branch.clone(),
        base: base.to_string(),
        head: head.to_string(),
        tier,
        score,
        signals,
        files,
        excluded,
        suggested_plan,
        config_path: cfg_path.display().to_string(),
    })
}

fn kind_str(k: &PathKind) -> &'static str {
    match k {
        PathKind::Source => "source",
        PathKind::Test => "test",
        PathKind::Doc => "doc",
        PathKind::Noise => "noise",
        PathKind::Config => "config",
        PathKind::Other => "other",
    }
}
