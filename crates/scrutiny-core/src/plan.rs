use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::eval::EvalReport;
use crate::paths::{temp_artifact_path, write_json_pretty};
use crate::scan::ScanReport;
use crate::score::Tier;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmedPlan {
    pub version: u32,
    pub client: String,
    pub model: String,
    pub security: bool,
    pub performance: bool,
    pub error_handling: bool,
    pub reviewers: u32,
    pub evangelists: u32,
    pub skip_ai: bool,
    pub skip_ai_reason: Option<String>,
    pub eval_path: String,
    pub map_path: Option<String>,
    pub pack_path: Option<String>,
    pub scan_path: Option<String>,
    /// Cap reviewers by pack size (skill should honor).
    pub max_reviewers: u32,
    pub spawn_evangelists: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanWriteInput {
    pub client: String,
    pub model: String,
    pub security: bool,
    pub performance: bool,
    pub error_handling: bool,
    pub reviewers: u32,
    pub evangelists: u32,
    pub eval_path: PathBuf,
    pub map_path: Option<PathBuf>,
    pub pack_path: Option<PathBuf>,
    pub scan_path: Option<PathBuf>,
}

pub fn run_plan_write(input: PlanWriteInput) -> Result<(ConfirmedPlan, PathBuf)> {
    let eval: EvalReport = serde_json::from_str(
        &fs::read_to_string(&input.eval_path)
            .with_context(|| format!("read eval {}", input.eval_path.display()))?,
    )
    .context("parse eval json")?;

    let scan: Option<ScanReport> = if let Some(p) = &input.scan_path {
        Some(
            serde_json::from_str(
                &fs::read_to_string(p).with_context(|| format!("read scan {}", p.display()))?,
            )
            .context("parse scan json")?,
        )
    } else {
        None
    };

    let pack_chars = if let Some(p) = &input.pack_path {
        let v: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(p).with_context(|| format!("read pack {}", p.display()))?,
        )
        .context("parse pack json")?;
        v.get("chars_used")
            .and_then(|c| c.as_u64())
            .unwrap_or(0) as usize
    } else {
        usize::MAX
    };

    let architecture_risk = scan
        .as_ref()
        .map(|s| s.architecture_risk)
        .unwrap_or(false);

    let (skip_ai, skip_ai_reason) = compute_skip_ai(
        eval.tier,
        &eval.signals.change_class,
        scan.as_ref(),
        input.reviewers,
        input.evangelists,
    );

    let mut reviewers = input.reviewers;
    let mut evangelists = input.evangelists;

    // Cap agents by pack size
    let max_reviewers = if pack_chars < 4_000 {
        1
    } else {
        reviewers.max(1)
    };
    if reviewers > max_reviewers {
        reviewers = max_reviewers;
    }

    // Evangelists only if architecture_risk or tier >= L
    let spawn_evangelists =
        evangelists > 0 && (architecture_risk || matches!(eval.tier, Tier::L | Tier::Xl));
    if !spawn_evangelists {
        evangelists = 0;
    }

    if skip_ai {
        reviewers = 0;
        evangelists = 0;
    }

    let plan = ConfirmedPlan {
        version: 1,
        client: input.client,
        model: input.model,
        security: input.security,
        performance: input.performance,
        error_handling: input.error_handling,
        reviewers,
        evangelists,
        skip_ai,
        skip_ai_reason,
        eval_path: input.eval_path.display().to_string(),
        map_path: input.map_path.map(|p| p.display().to_string()),
        pack_path: input.pack_path.map(|p| p.display().to_string()),
        scan_path: input.scan_path.map(|p| p.display().to_string()),
        max_reviewers,
        spawn_evangelists,
    };

    let out = temp_artifact_path(&eval.repo, &eval.branch, "plan");
    write_json_pretty(&out, &plan)?;
    Ok((plan, out))
}

/// XS + docs (+ empty scan) or zero agents with only-static path → skip AI.
pub fn compute_skip_ai(
    tier: Tier,
    change_class: &str,
    scan: Option<&ScanReport>,
    reviewers: u32,
    evangelists: u32,
) -> (bool, Option<String>) {
    let scan_empty = scan.map(|s| s.findings.is_empty()).unwrap_or(true);
    let docs_only = change_class.eq_ignore_ascii_case("docs")
        || change_class.eq_ignore_ascii_case("doc");

    if tier == Tier::Xs && docs_only && scan_empty {
        return (
            true,
            Some("tier XS + docs-only + empty scan — static clean; skip AI review".into()),
        );
    }

    if reviewers == 0 && evangelists == 0 {
        return (
            true,
            Some("reviewers=0 and evangelists=0 — use scan findings only".into()),
        );
    }

    (false, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xs_docs_empty_skips() {
        let (skip, reason) = compute_skip_ai(Tier::Xs, "docs", None, 1, 1);
        assert!(skip);
        assert!(reason.unwrap().contains("XS"));
    }

    #[test]
    fn zero_agents_skips() {
        let (skip, _) = compute_skip_ai(Tier::M, "feature", None, 0, 0);
        assert!(skip);
    }

    #[test]
    fn m_with_agents_runs() {
        let (skip, _) = compute_skip_ai(Tier::M, "feature", None, 1, 0);
        assert!(!skip);
    }
}
