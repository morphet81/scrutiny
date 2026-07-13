use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::forge::context::ForgeContextReport;
use crate::forge::fetch::TicketReport;
use crate::forge::plan::ForgeSessionPlan;
use crate::paths::{temp_artifact_path, write_json_pretty};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeBriefReport {
    pub version: u32,
    pub ticket_path: String,
    pub session_path: Option<String>,
    pub context_path: Option<String>,
    pub markdown_path: String,
    pub markdown: String,
}

pub fn run_forge_brief(
    ticket_path: &Path,
    session_path: Option<&Path>,
    context_path: Option<&Path>,
) -> Result<(ForgeBriefReport, PathBuf)> {
    let ticket: TicketReport = serde_json::from_str(
        &fs::read_to_string(ticket_path)
            .with_context(|| format!("read ticket {}", ticket_path.display()))?,
    )
    .context("parse ticket json")?;

    let session: Option<ForgeSessionPlan> = if let Some(p) = session_path {
        Some(
            serde_json::from_str(
                &fs::read_to_string(p).with_context(|| format!("read session {}", p.display()))?,
            )
            .context("parse session json")?,
        )
    } else {
        None
    };

    let context: Option<ForgeContextReport> = if let Some(p) = context_path {
        Some(
            serde_json::from_str(
                &fs::read_to_string(p).with_context(|| format!("read context {}", p.display()))?,
            )
            .context("parse context json")?,
        )
    } else {
        None
    };

    let markdown = render_brief(&ticket, session.as_ref(), context.as_ref());
    let md_path = temp_artifact_path("forge", &ticket.id, "brief").with_extension("md");
    fs::write(&md_path, &markdown).with_context(|| format!("write {}", md_path.display()))?;

    let report = ForgeBriefReport {
        version: 1,
        ticket_path: ticket_path.display().to_string(),
        session_path: session_path.map(|p| p.display().to_string()),
        context_path: context_path.map(|p| p.display().to_string()),
        markdown_path: md_path.display().to_string(),
        markdown: markdown.clone(),
    };
    let path = temp_artifact_path("forge", &ticket.id, "brief");
    write_json_pretty(&path, &report)?;
    Ok((report, path))
}

fn render_brief(
    ticket: &TicketReport,
    session: Option<&ForgeSessionPlan>,
    context: Option<&ForgeContextReport>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Forge brief: {} — {}\n\n",
        ticket.id, ticket.title
    ));
    out.push_str(&format!("Source: {}\n", ticket.source));
    if let Some(url) = &ticket.url {
        out.push_str(&format!("URL: {url}\n"));
    }
    if !ticket.labels.is_empty() {
        out.push_str(&format!("Labels: {}\n", ticket.labels.join(", ")));
    }
    out.push('\n');
    out.push_str("## Goal\n");
    out.push_str(&truncate(&ticket.description, 800));
    out.push_str("\n\n");

    if let Some(s) = session {
        out.push_str("## Session\n");
        out.push_str(&format!(
            "Approach: {} | Model: {} | Agents: {} | Testers: {} | E2E: {}\n",
            s.approach, s.model, s.agents, s.testers, s.e2e
        ));
        out.push_str(&format!(
            "Post review: reviewers={} evangelists={} | PO={} Figma={} Lore={} Writeback={}\n\n",
            s.reviewers,
            s.evangelists,
            s.enable_po,
            s.enable_figma,
            s.enable_lore,
            s.enable_ticket_writeback
        ));
    }

    if let Some(c) = context {
        out.push_str("## Context hits\n");
        if !c.keywords.is_empty() {
            out.push_str(&format!("Keywords: {}\n", c.keywords.join(", ")));
        }
        if let Some(u) = &c.test_harness.unit_framework {
            out.push_str(&format!("Unit: {u}\n"));
        }
        if let Some(e) = &c.test_harness.e2e_framework {
            out.push_str(&format!("E2E: {e}\n"));
        }
        if !c.test_harness.test_dirs.is_empty() {
            out.push_str(&format!(
                "Test dirs: {}\n",
                c.test_harness.test_dirs.join(", ")
            ));
        }
        out.push_str("Related paths (cap 20):\n");
        for p in c.related_paths.iter().take(20) {
            out.push_str(&format!("- {p}\n"));
        }
        out.push('\n');
    }

    if !ticket.comments.is_empty() {
        out.push_str("## Comments (last 3)\n");
        for c in ticket.comments.iter().rev().take(3).collect::<Vec<_>>().into_iter().rev()
        {
            out.push_str(&format!(
                "- {}: {}\n",
                c.author,
                truncate(&c.body, 200)
            ));
        }
        out.push('\n');
    }

    out.push_str("## Agent rules\n");
    out.push_str("- Read ticket/session/brief paths only. No re-fetch acli/gh/glab.\n");
    out.push_str("- Caveman I/O. Partition workstreams. No full-repo fish.\n");
    out
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}
