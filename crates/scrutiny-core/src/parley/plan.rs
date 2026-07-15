//! Parley knobs + comment partition.

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Input, Select};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::parley::fetch::{ParleyComment, ParleyCommentsFile};
use crate::paths::{artifact_path, write_json_pretty};
use crate::runtime::normalize_spawn_mode;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParleyAnswers {
    pub client: String,
    pub model: String,
    #[serde(default = "default_members")]
    pub members: u32,
    #[serde(default = "default_verifiers")]
    pub verifiers: u32,
    #[serde(default = "default_evangelists")]
    pub evangelists: u32,
    #[serde(default = "default_isolated")]
    pub spawn_mode: String,
}

fn default_members() -> u32 {
    1
}
fn default_verifiers() -> u32 {
    1
}
fn default_evangelists() -> u32 {
    1
}
fn default_isolated() -> String {
    "isolated".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParleyPlan {
    #[serde(default = "default_plan_version")]
    pub version: u32,
    pub client: String,
    pub model: String,
    pub members: u32,
    pub members_requested: u32,
    #[serde(default = "default_verifiers")]
    pub verifiers: u32,
    pub evangelists: u32,
    pub spawn_mode: String,
    pub pr_number: u64,
    pub comment_count: u32,
    /// Buckets of thread ids (same order as members).
    pub buckets: Vec<Vec<String>>,
    pub comments_path: String,
    pub fixes_path: String,
}

fn default_plan_version() -> u32 {
    1
}

#[derive(Debug, Clone)]
pub struct ParleyPlanWriteInput {
    pub comments: ParleyCommentsFile,
    pub comments_path: PathBuf,
    pub answers: ParleyAnswers,
    pub cfg: Config,
}

pub fn prompt_parley_answers(
    cfg: &Config,
    client: &str,
    model: &str,
    comment_count: usize,
    spawn_mode_preset: Option<&str>,
    from_json: Option<&str>,
    skip_prompt: bool,
) -> Result<ParleyAnswers> {
    if let Some(raw) = from_json {
        let mut a: ParleyAnswers =
            serde_json::from_str(raw).context("parse parley --from-json")?;
        a.spawn_mode = normalize_spawn_mode(&a.spawn_mode)?;
        a.members = a.members.max(1);
        if comment_count > 0 {
            a.members = a.members.min(comment_count as u32);
        } else {
            a.members = 0;
        }
        a.evangelists = a.evangelists.min(cfg.agents.max_evangelists);
        a.verifiers = a.verifiers.min(cfg.agents.max_evangelists);
        if a.client.is_empty() {
            a.client = client.to_string();
        }
        if a.model.is_empty() {
            a.model = model.to_string();
        }
        return Ok(a);
    }

    let max_members = comment_count.max(1) as u32;
    let default_members = cfg
        .parley
        .default_members
        .max(1)
        .min(max_members);
    let default_evangelists = cfg
        .parley
        .default_evangelists
        .min(cfg.agents.max_evangelists);
    let default_verifiers = cfg
        .parley
        .default_verifiers
        .min(cfg.agents.max_evangelists);

    if skip_prompt || !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        let spawn = if let Some(m) = spawn_mode_preset {
            normalize_spawn_mode(m)?
        } else if let Some(m) = &cfg.force_spawn_mode {
            normalize_spawn_mode(m)?
        } else {
            "isolated".into()
        };
        return Ok(ParleyAnswers {
            client: client.to_string(),
            model: model.to_string(),
            members: if comment_count == 0 {
                0
            } else {
                default_members
            },
            verifiers: default_verifiers,
            evangelists: default_evangelists,
            spawn_mode: spawn,
        });
    }

    let members: u32 = Input::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "Team members (1–{max_members}; cannot exceed comment count)"
        ))
        .default(default_members)
        .validate_with(|n: &u32| -> Result<(), String> {
            if *n < 1 || *n > max_members {
                return Err(format!("must be 1..={max_members}"));
            }
            Ok(())
        })
        .interact_text()
        .context("members prompt")?;

    let verifiers: u32 = Input::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "Verifiers to check fixes address comments (0–{})",
            cfg.agents.max_evangelists
        ))
        .default(default_verifiers)
        .validate_with(|n: &u32| -> Result<(), String> {
            if *n > cfg.agents.max_evangelists {
                return Err(format!("max {}", cfg.agents.max_evangelists));
            }
            Ok(())
        })
        .interact_text()
        .context("verifiers prompt")?;

    let evangelists: u32 = Input::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "Evangelists to verify fixes afterwards (0–{})",
            cfg.agents.max_evangelists
        ))
        .default(default_evangelists)
        .validate_with(|n: &u32| -> Result<(), String> {
            if *n > cfg.agents.max_evangelists {
                return Err(format!("max {}", cfg.agents.max_evangelists));
            }
            Ok(())
        })
        .interact_text()
        .context("evangelists prompt")?;

    let spawn_mode = if let Some(m) = spawn_mode_preset {
        normalize_spawn_mode(m)?
    } else if let Some(m) = &cfg.force_spawn_mode {
        normalize_spawn_mode(m)?
    } else {
        let items = [
            "isolated — script runs members in parallel (recommended)",
            "team — one lead agent spawns members",
        ];
        let sel = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Spawn mode")
            .items(&items)
            .default(0)
            .interact()
            .context("spawn mode")?;
        if sel == 0 {
            "isolated".into()
        } else {
            "team".into()
        }
    };

    Ok(ParleyAnswers {
        client: client.to_string(),
        model: model.to_string(),
        members,
        verifiers,
        evangelists,
        spawn_mode,
    })
}

pub fn run_parley_plan_write(input: ParleyPlanWriteInput) -> Result<(ParleyPlan, PathBuf)> {
    let n = input.comments.comments.len();
    if n == 0 {
        bail!("parley-plan-write: no comments to address");
    }
    let requested = input.answers.members.max(1);
    let members = requested.min(n as u32);
    if requested > n as u32 {
        eprintln!(
            "scrutiny parley: members requested {requested} > {n} comments — using {members}"
        );
    }
    let evangelists = input
        .answers
        .evangelists
        .min(input.cfg.agents.max_evangelists);
    let verifiers = input
        .answers
        .verifiers
        .min(input.cfg.agents.max_evangelists);
    let spawn_mode = normalize_spawn_mode(&input.answers.spawn_mode)?;
    let buckets = partition_comments(&input.comments.comments, members);
    let fixes_path = artifact_path("parley-fixes");
    let plan = ParleyPlan {
        version: 1,
        client: input.answers.client.clone(),
        model: input.answers.model.clone(),
        members,
        members_requested: requested,
        verifiers,
        evangelists,
        spawn_mode,
        pr_number: input.comments.pr_number,
        comment_count: n as u32,
        buckets,
        comments_path: input.comments_path.display().to_string(),
        fixes_path: fixes_path.display().to_string(),
    };
    let path = artifact_path("parley-plan");
    write_json_pretty(&path, &plan)?;
    Ok((plan, path))
}

/// Path-affinity then round-robin fill across `n` buckets.
pub fn partition_comments(comments: &[ParleyComment], n: u32) -> Vec<Vec<String>> {
    let n = n.max(1) as usize;
    let mut buckets: Vec<Vec<String>> = (0..n).map(|_| Vec::new()).collect();
    if comments.is_empty() {
        return buckets;
    }

    // Group by path (empty path → "_")
    let mut by_path: HashMap<String, Vec<String>> = HashMap::new();
    let mut path_order: Vec<String> = Vec::new();
    for c in comments {
        let key = if c.path.is_empty() {
            "_".into()
        } else {
            c.path.clone()
        };
        if !by_path.contains_key(&key) {
            path_order.push(key.clone());
        }
        by_path.entry(key).or_default().push(c.id.clone());
    }

    // Sort path groups by size desc so bigger groups land first
    path_order.sort_by(|a, b| {
        by_path
            .get(b)
            .map(|v| v.len())
            .unwrap_or(0)
            .cmp(&by_path.get(a).map(|v| v.len()).unwrap_or(0))
            .then_with(|| a.cmp(b))
    });

    for path in path_order {
        let ids = by_path.remove(&path).unwrap_or_default();
        // Pick bucket with fewest ids
        let mut target = 0usize;
        let mut min_len = buckets[0].len();
        for (i, b) in buckets.iter().enumerate().skip(1) {
            if b.len() < min_len {
                min_len = b.len();
                target = i;
            }
        }
        buckets[target].extend(ids);
    }

    // Drop empty trailing buckets (should not happen if n ≤ comments)
    while buckets.len() > 1 && buckets.last().map(|b| b.is_empty()).unwrap_or(false) {
        buckets.pop();
    }
    buckets
}

pub fn load_parley_plan(path: &Path) -> Result<ParleyPlan> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

pub fn load_parley_comments(path: &Path) -> Result<ParleyCommentsFile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parley::fetch::ParleyComment;

    fn c(id: &str, path: &str) -> ParleyComment {
        ParleyComment {
            id: id.into(),
            comment_id: String::new(),
            database_id: None,
            path: path.into(),
            line: Some(1),
            start_line: None,
            side: None,
            diff_side: None,
            is_outdated: false,
            author: "r".into(),
            body: "x".into(),
            url: String::new(),
            comments: vec![],
        }
    }

    #[test]
    fn verifiers_round_trip_from_json() {
        let cfg: Config = toml::from_str(crate::config::DEFAULT_TOML).unwrap();
        let raw = r#"{"client":"claude","model":"opus","members":1,"verifiers":2,"evangelists":0,"spawn_mode":"isolated"}"#;
        let a = prompt_parley_answers(&cfg, "claude", "opus", 3, None, Some(raw), true).unwrap();
        assert_eq!(a.verifiers, 2.min(cfg.agents.max_evangelists));
    }

    #[test]
    fn verifiers_default_when_absent_from_json() {
        let cfg: Config = toml::from_str(crate::config::DEFAULT_TOML).unwrap();
        let raw = r#"{"client":"claude","model":"opus","members":1,"spawn_mode":"isolated"}"#;
        let a = prompt_parley_answers(&cfg, "claude", "opus", 3, None, Some(raw), true).unwrap();
        assert_eq!(a.verifiers, default_verifiers().min(cfg.agents.max_evangelists));
    }

    #[test]
    fn partition_caps_and_groups() {
        let comments = vec![
            c("a", "foo.rs"),
            c("b", "foo.rs"),
            c("c", "bar.rs"),
        ];
        let buckets = partition_comments(&comments, 2);
        assert_eq!(buckets.len(), 2);
        let total: usize = buckets.iter().map(|b| b.len()).sum();
        assert_eq!(total, 3);
        // foo.rs pair should stay together
        let foo_bucket = buckets
            .iter()
            .find(|b| b.contains(&"a".into()) && b.contains(&"b".into()));
        assert!(foo_bucket.is_some());
    }

    #[test]
    fn members_cannot_exceed_via_answers_cap() {
        let comments = vec![c("a", "x"), c("b", "y")];
        let buckets = partition_comments(&comments, 99.min(comments.len() as u32));
        assert_eq!(buckets.len(), 2);
    }
}
