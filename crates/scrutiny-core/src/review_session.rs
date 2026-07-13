//! Review session artifact — records spawned reviewer/evangelist agents for quality gates.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::pack::PackReport;
use crate::paths::{temp_artifact_path, write_json_pretty};
use crate::plan::ConfirmedPlan;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewSession {
    pub version: u32,
    pub plan_path: String,
    pub pack_path: Option<String>,
    pub model: String,
    pub reviewers_expected: u32,
    pub evangelists_expected: u32,
    pub agents: Vec<ReviewAgentRecord>,
    pub valid: bool,
    pub validation_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewAgentRecord {
    pub role: String,
    pub index: u32,
    pub paths: Vec<String>,
    pub findings_count: u32,
}

#[derive(Debug, Clone)]
pub struct ReviewSessionWriteInput {
    pub plan_path: PathBuf,
    pub pack_path: Option<PathBuf>,
    /// JSON array of agents (or full object with agents key).
    pub from_json: String,
}

pub fn run_review_session_write(
    input: ReviewSessionWriteInput,
) -> Result<(ReviewSession, PathBuf)> {
    let plan: ConfirmedPlan = serde_json::from_str(
        &fs::read_to_string(&input.plan_path)
            .with_context(|| format!("read plan {}", input.plan_path.display()))?,
    )
    .context("parse plan json")?;

    let pack_path = input
        .pack_path
        .map(|p| p.display().to_string())
        .or(plan.pack_path.clone());

    let agents = parse_agents_json(&input.from_json)?;

    let reviewers_expected = plan.reviewers;
    let evangelists_expected = plan.evangelists;

    let mut errs = Vec::new();
    let reviewer_count = agents.iter().filter(|a| a.role == "reviewer").count() as u32;
    let evangelist_count = agents.iter().filter(|a| a.role == "evangelist").count() as u32;

    if !plan.skip_ai {
        if reviewer_count != reviewers_expected {
            errs.push(format!(
                "reviewers_expected={reviewers_expected} but agents has {reviewer_count} reviewer(s)"
            ));
        }
        if evangelist_count != evangelists_expected {
            errs.push(format!(
                "evangelists_expected={evangelists_expected} but agents has {evangelist_count} evangelist(s)"
            ));
        }
        let expected_total = reviewers_expected + evangelists_expected;
        if agents.len() as u32 != expected_total {
            errs.push(format!(
                "agents.length={} != reviewers+evangelists ({expected_total})",
                agents.len()
            ));
        }
    }

    if reviewers_expected > 0 && plan.reviewers_requested > plan.max_reviewers {
        // informational — not a hard error; already capped in plan
        let _ = plan.reviewers_requested;
    }

    let session = ReviewSession {
        version: 1,
        plan_path: input.plan_path.display().to_string(),
        pack_path,
        model: plan.model.clone(),
        reviewers_expected,
        evangelists_expected,
        agents,
        valid: errs.is_empty(),
        validation_errors: errs.clone(),
    };

    let repo = Path::new(&plan.eval_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("repo");
    let out = temp_artifact_path(repo, "review", "session");
    write_json_pretty(&out, &session)?;

    if !session.valid {
        bail!(
            "review-session-write invalid:\n  - {}",
            session.validation_errors.join("\n  - ")
        );
    }
    Ok((session, out))
}

fn parse_agents_json(raw: &str) -> Result<Vec<ReviewAgentRecord>> {
    let v: serde_json::Value =
        serde_json::from_str(raw).context("parse review-session --from-json")?;
    if let Some(arr) = v.as_array() {
        return Ok(serde_json::from_value(serde_json::Value::Array(arr.clone()))
            .context("parse agents array")?);
    }
    if let Some(agents) = v.get("agents") {
        return Ok(serde_json::from_value(agents.clone()).context("parse agents field")?);
    }
    bail!("--from-json must be an agents array or object with agents key");
}

/// Partition pack slice paths across `n` reviewer buckets (round-robin by path).
pub fn partition_pack_paths(pack_path: &Path, n: u32) -> Result<Vec<Vec<String>>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let pack: PackReport = serde_json::from_str(
        &fs::read_to_string(pack_path)
            .with_context(|| format!("read pack {}", pack_path.display()))?,
    )
    .context("parse pack")?;
    let mut paths: Vec<String> = pack.slices.iter().map(|s| s.path.clone()).collect();
    paths.sort();
    paths.dedup();

    let mut buckets = vec![Vec::new(); n as usize];
    for (i, p) in paths.into_iter().enumerate() {
        buckets[i % n as usize].push(p);
    }
    Ok(buckets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agents_array() {
        let raw = r#"[{"role":"reviewer","index":1,"paths":["a.rs"],"findings_count":2}]"#;
        let a = parse_agents_json(raw).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].role, "reviewer");
    }

    #[test]
    fn partition_round_robin() {
        let dir = tempfile::tempdir().unwrap();
        let pack_path = dir.path().join("pack.json");
        let pack = serde_json::json!({
            "version": 1,
            "map_path": "m",
            "repo": "r",
            "branch": "b",
            "base": "main",
            "head": "HEAD",
            "tier": "M",
            "max_chars": 1000,
            "chars_used": 100,
            "truncated": false,
            "architecture_risk": false,
            "needs_full_file": [],
            "slices": [
                {"path": "a.rs", "kind": "diff", "unified_diff": "", "symbol_slices": []},
                {"path": "b.rs", "kind": "diff", "unified_diff": "", "symbol_slices": []},
                {"path": "c.rs", "kind": "diff", "unified_diff": "", "symbol_slices": []}
            ],
            "doc_digests": [],
            "markdown_path": null
        });
        std::fs::write(&pack_path, pack.to_string()).unwrap();
        let buckets = partition_pack_paths(&pack_path, 2).unwrap();
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0], vec!["a.rs", "c.rs"]);
        assert_eq!(buckets[1], vec!["b.rs"]);
    }
}
