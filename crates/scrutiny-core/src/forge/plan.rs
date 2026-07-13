use anyhow::{Context, Result};
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

    // Prefer explicit flags; fall back to config feature toggles
    let enable_figma = cfg.forge.enable_figma;
    let enable_lore = cfg.forge.enable_lore;
    let enable_ticket_writeback = cfg.forge.enable_ticket_writeback;
    let enable_po = cfg.forge.enable_po;

    let approach = normalize_approach(&input.approach)?;
    let mut reviewers = input.reviewers;
    let mut evangelists = input.evangelists;
    let (skip_ai_review, skip_ai_review_reason) =
        if reviewers == 0 && evangelists == 0 {
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
    };

    let path = temp_artifact_path("forge", &ticket.id, "session");
    write_json_pretty(&path, &plan)?;
    // silence unused when ticket only used for id
    let _ = ticket.title;
    Ok((plan, path))
}

fn normalize_approach(raw: &str) -> Result<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "tdd" => Ok("tdd".into()),
        "heads_down" | "heads-down" | "headless" | "auto" => Ok("heads_down".into()),
        "plan" | "plan_mode" | "plan-mode" => Ok("plan".into()),
        other => anyhow::bail!("unknown approach {other} (tdd|heads_down|plan)"),
    }
}
