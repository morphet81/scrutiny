use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::config::{ensure_config, find_shipped_default, load_config};
use crate::forge::fetch::TicketReport;
use crate::paths::{temp_artifact_path, write_json_pretty};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeSessionPlan {
    pub version: u32,
    pub client: String,
    pub model: String,
    pub approach: String,
    pub e2e: bool,
    pub agents: u32,
    pub testers: u32,
    pub reviewers: u32,
    pub evangelists: u32,
    pub enable_figma: bool,
    pub enable_lore: bool,
    pub enable_ticket_writeback: bool,
    pub enable_po: bool,
    pub ticket_path: String,
    pub skip_ai_review: bool,
    pub skip_ai_review_reason: Option<String>,
    /// single (default) | team
    #[serde(default = "default_spawn_single")]
    pub spawn_mode: String,
    #[serde(default)]
    pub use_playwright: bool,
    #[serde(default = "default_coverage")]
    pub coverage_pct: u32,
    #[serde(default = "default_true")]
    pub tdd: bool,
    #[serde(default)]
    pub tdd_plan_path: Option<String>,
    #[serde(default)]
    pub figma_dir: Option<String>,
}

fn default_spawn_single() -> String {
    "single".into()
}
fn default_coverage() -> u32 {
    100
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgePlanWriteInput {
    pub ticket_path: PathBuf,
    pub client: String,
    pub model: String,
    pub approach: String,
    pub e2e: bool,
    pub agents: u32,
    pub testers: u32,
    pub reviewers: u32,
    pub evangelists: u32,
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_spawn_single")]
    pub spawn_mode: String,
    #[serde(default)]
    pub use_playwright: bool,
    #[serde(default = "default_coverage")]
    pub coverage_pct: u32,
    #[serde(default = "default_true")]
    pub tdd: bool,
    #[serde(default)]
    pub tdd_plan_path: Option<String>,
    #[serde(default)]
    pub figma_dir: Option<String>,
}

pub fn run_forge_plan_write(input: ForgePlanWriteInput) -> Result<(ForgeSessionPlan, PathBuf)> {
    let ticket: TicketReport = serde_json::from_str(
        &fs::read_to_string(&input.ticket_path)
            .with_context(|| format!("read ticket {}", input.ticket_path.display()))?,
    )
    .context("parse ticket json")?;

    let cwd = input
        .cwd
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let shipped = find_shipped_default(&cwd);
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;

    let enable_figma = cfg.forge.enable_figma;
    let enable_lore = cfg.forge.enable_lore;
    let enable_ticket_writeback = cfg.forge.enable_ticket_writeback;
    let enable_po = cfg.forge.enable_po;

    let approach = if input.tdd {
        "tdd".into()
    } else {
        "heads_down".into()
    };

    let mut reviewers = input.reviewers;
    let mut evangelists = input.evangelists;
    let (skip_ai_review, skip_ai_review_reason) = if reviewers == 0 && evangelists == 0 {
        (
            true,
            Some("reviewers=evangelists=0; skip post-impl AI review".into()),
        )
    } else {
        (false, None)
    };
    if skip_ai_review {
        reviewers = 0;
        evangelists = 0;
    }

    let spawn_mode = normalize_spawn(&input.spawn_mode)?;

    let plan = ForgeSessionPlan {
        version: 1,
        client: input.client,
        model: input.model,
        approach,
        e2e: input.e2e,
        agents: input.agents.max(1),
        testers: input.testers,
        reviewers,
        evangelists,
        enable_figma,
        enable_lore,
        enable_ticket_writeback,
        enable_po,
        ticket_path: input.ticket_path.display().to_string(),
        skip_ai_review,
        skip_ai_review_reason,
        spawn_mode,
        use_playwright: input.use_playwright,
        coverage_pct: input.coverage_pct.min(100),
        tdd: input.tdd,
        tdd_plan_path: input.tdd_plan_path,
        figma_dir: input.figma_dir.or(ticket.figma_dir.clone()),
    };

    let _ = crate::paths::init_artifact_ctx(
        &cwd,
        &crate::paths::session_name(None, Some(&ticket.id)),
    );
    let path = temp_artifact_path("forge", &ticket.id, "session");
    write_json_pretty(&path, &plan)?;
    Ok((plan, path))
}

fn normalize_spawn(raw: &str) -> Result<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "single" | "solo" | "isolated" => Ok("single".into()),
        "team" => Ok("team".into()),
        other => bail!("spawn_mode must be single|team, got {other}"),
    }
}
