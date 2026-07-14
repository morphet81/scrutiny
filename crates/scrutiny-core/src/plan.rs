use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
    /// Effective reviewer count after caps / skip_ai.
    pub reviewers: u32,
    /// Effective evangelist count after spawn rules / skip_ai.
    pub evangelists: u32,
    /// User-requested reviewers before pack cap / skip_ai.
    #[serde(default)]
    pub reviewers_requested: u32,
    /// User-requested evangelists before spawn rules / skip_ai.
    #[serde(default)]
    pub evangelists_requested: u32,
    pub skip_ai: bool,
    pub skip_ai_reason: Option<String>,
    pub eval_path: String,
    pub map_path: Option<String>,
    pub pack_path: Option<String>,
    pub scan_path: Option<String>,
    /// Cap reviewers by pack size (skill should honor).
    pub max_reviewers: u32,
    pub spawn_evangelists: bool,
    /// isolated (script parallel) | team (one lead spawns team).
    #[serde(default = "default_spawn_team")]
    pub spawn_mode: String,
}

fn default_spawn_team() -> String {
    "team".into()
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
    #[serde(default = "default_spawn_team")]
    pub spawn_mode: String,
    pub eval_path: PathBuf,
    pub map_path: Option<PathBuf>,
    pub pack_path: Option<PathBuf>,
    pub scan_path: Option<PathBuf>,
}

/// Interactive / non-interactive answers from `plan-confirm` (fed into plan-write).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanAnswers {
    pub client: String,
    pub model: String,
    pub security: bool,
    pub performance: bool,
    pub error_handling: bool,
    pub reviewers: u32,
    pub evangelists: u32,
    #[serde(default = "default_spawn_team")]
    pub spawn_mode: String,
}

#[derive(Debug, Clone)]
pub struct PlanConfirmInput {
    pub eval_path: PathBuf,
    pub client: Option<String>,
    /// Pre-resolved spawn mode (from review command / config); skips that prompt when set.
    pub spawn_mode: Option<String>,
    /// When set, skip stdin and use these answers (CI / tests).
    pub from_json: Option<String>,
}

pub fn run_plan_confirm(input: PlanConfirmInput) -> Result<(PlanAnswers, PathBuf)> {
    let eval: EvalReport = serde_json::from_str(
        &fs::read_to_string(&input.eval_path)
            .with_context(|| format!("read eval {}", input.eval_path.display()))?,
    )
    .context("parse eval json")?;

    let suggested = &eval.suggested_plan;
    let client = input
        .client
        .clone()
        .unwrap_or_else(|| suggested.client.clone());

    let answers = if let Some(raw) = &input.from_json {
        let mut a: PlanAnswers =
            serde_json::from_str(raw).context("parse plan-confirm --from-json")?;
        if a.client.is_empty() {
            a.client = client;
        }
        if let Some(m) = &input.spawn_mode {
            a.spawn_mode = crate::runtime::normalize_spawn_mode(m)?;
        } else {
            a.spawn_mode = crate::runtime::normalize_spawn_mode(&a.spawn_mode)?;
        }
        a
    } else {
        prompt_plan_answers(&client, suggested, input.spawn_mode.as_deref())?
    };

    let out = temp_artifact_path(&eval.repo, &eval.branch, "plan-answers");
    write_json_pretty(&out, &answers)?;
    Ok((answers, out))
}

fn prompt_plan_answers(
    client: &str,
    suggested: &crate::config::SuggestedPlan,
    spawn_mode_preset: Option<&str>,
) -> Result<PlanAnswers> {
    use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
    use std::io::IsTerminal;

    if !io::stdin().is_terminal() {
        bail!(
            "plan-confirm needs an interactive TTY (or pass --from-json with explicit answers).\n\
             Empty/non-TTY stdin would silently accept suggested defaults — that is forbidden.\n\
             Run in a real terminal, or:\n\
               scrutiny plan-confirm --eval <eval.json> --from-json '{{...}}'"
        );
    }

    let theme = ColorfulTheme::default();
    eprintln!("scrutiny plan-confirm: ↑/↓ select, Enter confirm.");
    eprintln!("Client: {client}");
    eprintln!();

    let models = if suggested.available_models.is_empty() {
        vec![suggested.model.clone()]
    } else {
        suggested.available_models.clone()
    };

    let default_model_idx = models
        .iter()
        .position(|m| m == &suggested.model)
        .unwrap_or(0);
    let model_labels: Vec<String> = models
        .iter()
        .map(|m| {
            if m == &suggested.model {
                format!("{m}  (recommended)")
            } else {
                m.clone()
            }
        })
        .collect();
    let model_sel = Select::with_theme(&theme)
        .with_prompt("1) Model")
        .items(&model_labels)
        .default(default_model_idx)
        .interact()
        .context("model menu")?;
    let model = models[model_sel].clone();

    let security = Confirm::with_theme(&theme)
        .with_prompt("2) Security analysis?")
        .default(suggested.security)
        .interact()
        .context("security confirm")?;
    let performance = Confirm::with_theme(&theme)
        .with_prompt("3) Performance analysis?")
        .default(suggested.performance)
        .interact()
        .context("performance confirm")?;
    let error_handling = Confirm::with_theme(&theme)
        .with_prompt("4) Error-handling analysis?")
        .default(suggested.error_handling)
        .interact()
        .context("error-handling confirm")?;

    let reviewers: u32 = Input::with_theme(&theme)
        .with_prompt("5) Reviewer agents (count)")
        .default(suggested.reviewers)
        .interact_text()
        .context("reviewers input")?;
    let evangelists: u32 = Input::with_theme(&theme)
        .with_prompt("6) Evangelist agents (count)")
        .default(suggested.evangelists)
        .interact_text()
        .context("evangelists input")?;

    let spawn_mode = if let Some(m) = spawn_mode_preset {
        crate::runtime::normalize_spawn_mode(m)?
    } else {
        let items = [
            "team — one lead agent spawns its own team",
            "isolated — parallel reviewers/evangelists/specialists",
        ];
        let sel = Select::with_theme(&theme)
            .with_prompt("7) Spawn mode")
            .items(&items)
            .default(0)
            .interact()
            .context("spawn mode menu")?;
        if sel == 0 {
            "team".into()
        } else {
            "isolated".into()
        }
    };

    Ok(PlanAnswers {
        client: client.to_string(),
        model,
        security,
        performance,
        error_handling,
        reviewers,
        evangelists,
        spawn_mode,
    })
}

pub fn load_plan_answers(path: &Path) -> Result<PlanAnswers> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
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

    let reviewers_requested = input.reviewers;
    let evangelists_requested = input.evangelists;

    let (skip_ai, skip_ai_reason) = compute_skip_ai(
        eval.tier,
        &eval.signals.change_class,
        scan.as_ref(),
        input.reviewers,
        input.evangelists,
        input.security,
        input.performance,
        input.error_handling,
        &input.spawn_mode,
    );

    let mut reviewers = input.reviewers;
    let mut evangelists = input.evangelists;
    let spawn_mode = crate::runtime::normalize_spawn_mode(&input.spawn_mode)
        .unwrap_or_else(|_| "team".into());

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
        security: input.security && !skip_ai,
        performance: input.performance && !skip_ai,
        error_handling: input.error_handling && !skip_ai,
        reviewers,
        evangelists,
        reviewers_requested,
        evangelists_requested,
        skip_ai,
        skip_ai_reason,
        eval_path: input.eval_path.display().to_string(),
        map_path: input.map_path.map(|p| p.display().to_string()),
        pack_path: input.pack_path.map(|p| p.display().to_string()),
        scan_path: input.scan_path.map(|p| p.display().to_string()),
        max_reviewers,
        spawn_evangelists,
        spawn_mode,
    };

    if reviewers_requested > max_reviewers {
        eprintln!(
            "scrutiny plan-write: reviewers capped {reviewers_requested} → {reviewers} (max_reviewers={max_reviewers}, pack_chars={pack_chars})"
        );
    }
    if evangelists_requested > 0 && !spawn_evangelists && !skip_ai {
        eprintln!(
            "scrutiny plan-write: evangelists capped {evangelists_requested} → 0 (need architecture_risk or tier L/XL)"
        );
    }
    if skip_ai {
        if let Some(r) = &plan.skip_ai_reason {
            eprintln!("scrutiny plan-write: skip_ai — {r}");
        }
    }

    let out = temp_artifact_path(&eval.repo, &eval.branch, "plan");
    write_json_pretty(&out, &plan)?;
    Ok((plan, out))
}

/// XS + docs (+ empty scan) or zero agents/specialists (and not team) → skip AI.
pub fn compute_skip_ai(
    tier: Tier,
    change_class: &str,
    scan: Option<&ScanReport>,
    reviewers: u32,
    evangelists: u32,
    security: bool,
    performance: bool,
    error_handling: bool,
    spawn_mode: &str,
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

    let specialists = security || performance || error_handling;
    let team = spawn_mode.eq_ignore_ascii_case("team");
    if reviewers == 0 && evangelists == 0 && !specialists && !team {
        return (
            true,
            Some("no reviewers/evangelists/specialists — use scan findings only".into()),
        );
    }

    (false, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xs_docs_empty_skips() {
        let (skip, reason) = compute_skip_ai(Tier::Xs, "docs", None, 1, 1, true, true, true, "isolated");
        assert!(skip);
        assert!(reason.unwrap().contains("XS"));
    }

    #[test]
    fn zero_agents_skips() {
        let (skip, _) = compute_skip_ai(Tier::M, "feature", None, 0, 0, false, false, false, "isolated");
        assert!(skip);
    }

    #[test]
    fn specialists_keep_ai() {
        let (skip, _) = compute_skip_ai(Tier::M, "feature", None, 0, 0, true, false, false, "isolated");
        assert!(!skip);
    }

    #[test]
    fn m_with_agents_runs() {
        let (skip, _) = compute_skip_ai(Tier::M, "feature", None, 1, 0, false, false, false, "isolated");
        assert!(!skip);
    }

    #[test]
    fn from_json_skips_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let eval_path = dir.path().join("eval.json");
        let eval = r#"{
            "version": 1,
            "mode": "local",
            "repo": "test/repo",
            "branch": "main",
            "base": "main",
            "head": "HEAD",
            "tier": "M",
            "score": 40,
            "signals": {
                "relevant_files": 1,
                "relevant_loc": 10,
                "added": 10,
                "deleted": 0,
                "scatter": 0.0,
                "blast_stub": 0,
                "risk_path_hits": 0,
                "layers_touched": [],
                "change_class": "feature"
            },
            "files": [],
            "excluded": [],
            "suggested_plan": {
                "client": "claude",
                "model": "sonnet",
                "available_models": ["haiku", "sonnet", "opus"],
                "security": true,
                "performance": false,
                "error_handling": true,
                "reviewers": 1,
                "evangelists": 0,
                "prompt_reviewers": true,
                "prompt_evangelists": false
            },
            "config_path": "/tmp/x"
        }"#;
        fs::write(&eval_path, eval).unwrap();

        let answers_json = r#"{
            "client": "claude",
            "model": "opus",
            "security": true,
            "performance": true,
            "error_handling": false,
            "reviewers": 2,
            "evangelists": 1
        }"#;

        let (answers, path) = run_plan_confirm(PlanConfirmInput {
            eval_path,
            client: None,
            spawn_mode: None,
            from_json: Some(answers_json.into()),
        })
        .unwrap();

        assert_eq!(answers.model, "opus");
        assert_eq!(answers.reviewers, 2);
        assert_eq!(answers.evangelists, 1);
        assert_eq!(answers.spawn_mode, "team");
        assert!(path.exists());
        assert!(!answers.error_handling);
        assert!(answers.performance);
    }
}
