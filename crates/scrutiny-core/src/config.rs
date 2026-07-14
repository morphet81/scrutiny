use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::score::Tier;

const CONFIG_DIR_NAME: &str = ".scrutiny";
const CONFIG_FILE_NAME: &str = "config.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub default_client: String,
    /// Force headless client for `scrutiny review` (cursor|claude|codex). Omit → detect + prompt.
    #[serde(default)]
    pub force_client: Option<String>,
    /// Force spawn mode: isolated | team. Omit → prompt (default team).
    #[serde(default)]
    pub force_spawn_mode: Option<String>,
    pub models: BTreeMap<String, ClientModels>,
    pub review: ReviewConfig,
    pub agents: AgentsConfig,
    pub git: GitConfig,
    #[serde(default)]
    pub pack: PackConfig,
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default)]
    pub forge: ForgeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeConfig {
    /// Force approach; omit → prompt. Values: tdd | heads_down | plan
    #[serde(default)]
    pub approach: Option<String>,
    #[serde(default)]
    pub e2e: Option<bool>,
    #[serde(default)]
    pub agents: Option<u32>,
    #[serde(default)]
    pub testers: Option<u32>,
    #[serde(default)]
    pub reviewers: Option<u32>,
    #[serde(default)]
    pub evangelists: Option<u32>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_true")]
    pub enable_figma: bool,
    #[serde(default = "default_true")]
    pub enable_lore: bool,
    #[serde(default = "default_true")]
    pub enable_ticket_writeback: bool,
    #[serde(default = "default_true")]
    pub enable_po: bool,
    #[serde(default = "default_approach_tdd")]
    pub default_approach: String,
    #[serde(default = "default_agents_2")]
    pub default_agents: u32,
    #[serde(default = "default_testers_1")]
    pub default_testers: u32,
    #[serde(default = "default_reviewers_1")]
    pub default_reviewers: u32,
    #[serde(default)]
    pub default_evangelists: u32,
    /// Explicit verify-gate commands (test/lint/build). Empty → auto-derive from harness.
    #[serde(default)]
    pub verify_commands: Vec<String>,
    /// Max fix-loops the verify gate runs before it stops and gates the commit.
    #[serde(default = "default_verify_loops")]
    pub verify_max_loops: u32,
    /// Gate on coverage % when measurable (auto-derived commands only).
    #[serde(default = "default_true")]
    pub verify_coverage: bool,
}

fn default_true() -> bool {
    true
}
fn default_approach_tdd() -> String {
    "tdd".into()
}
fn default_agents_2() -> u32 {
    2
}
fn default_testers_1() -> u32 {
    1
}
fn default_reviewers_1() -> u32 {
    1
}
fn default_verify_loops() -> u32 {
    2
}

impl Default for ForgeConfig {
    fn default() -> Self {
        Self {
            approach: None,
            e2e: None,
            agents: None,
            testers: None,
            reviewers: None,
            evangelists: None,
            model: None,
            enable_figma: true,
            enable_lore: true,
            enable_ticket_writeback: true,
            enable_po: true,
            default_approach: default_approach_tdd(),
            default_agents: default_agents_2(),
            default_testers: default_testers_1(),
            default_reviewers: default_reviewers_1(),
            default_evangelists: 0,
            verify_commands: Vec::new(),
            verify_max_loops: default_verify_loops(),
            verify_coverage: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientModels {
    pub xs: Option<String>,
    pub s: Option<String>,
    pub m: Option<String>,
    pub l: Option<String>,
    pub xl: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewConfig {
    pub security_by_tier: TierBools,
    pub performance_by_tier: TierBools,
    pub error_handling_by_tier: TierBools,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsConfig {
    pub reviewers_by_tier: TierCounts,
    pub evangelists_by_tier: TierCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    pub base_candidates: Vec<String>,
    pub exclude_globs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackConfig {
    pub max_chars: usize,
    pub doc_digest_lines: usize,
    pub symbol_context_lines: usize,
}

impl Default for PackConfig {
    fn default() -> Self {
        Self {
            max_chars: 48_000,
            doc_digest_lines: 40,
            symbol_context_lines: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanConfig {
    pub enable: bool,
    #[serde(default)]
    pub commands: Vec<String>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            enable: true,
            commands: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierBools {
    #[serde(rename = "XS")]
    pub xs: bool,
    #[serde(rename = "S")]
    pub s: bool,
    #[serde(rename = "M")]
    pub m: bool,
    #[serde(rename = "L")]
    pub l: bool,
    #[serde(rename = "XL")]
    pub xl: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierCounts {
    #[serde(rename = "XS")]
    pub xs: u32,
    #[serde(rename = "S")]
    pub s: u32,
    #[serde(rename = "M")]
    pub m: u32,
    #[serde(rename = "L")]
    pub l: u32,
    #[serde(rename = "XL")]
    pub xl: u32,
}

impl TierBools {
    pub fn get(&self, tier: Tier) -> bool {
        match tier {
            Tier::Xs => self.xs,
            Tier::S => self.s,
            Tier::M => self.m,
            Tier::L => self.l,
            Tier::Xl => self.xl,
        }
    }
}

impl TierCounts {
    pub fn get(&self, tier: Tier) -> u32 {
        match tier {
            Tier::Xs => self.xs,
            Tier::S => self.s,
            Tier::M => self.m,
            Tier::L => self.l,
            Tier::Xl => self.xl,
        }
    }
}

impl ClientModels {
    pub fn for_tier(&self, tier: Tier) -> Option<&str> {
        match tier {
            Tier::Xs => self.xs.as_deref(),
            Tier::S => self.s.as_deref(),
            Tier::M => self.m.as_deref(),
            Tier::L => self.l.as_deref(),
            Tier::Xl => self.xl.as_deref(),
        }
    }
}

impl Config {
    pub fn model_for(&self, client: &str, tier: Tier) -> Option<&str> {
        self.models
            .get(client)
            .and_then(|m| m.for_tier(tier))
            .or_else(|| {
                self.models
                    .get(&self.default_client)
                    .and_then(|m| m.for_tier(tier))
            })
    }

    /// Unique model ids configured for a client (xs→xl order, deduped).
    pub fn available_models(&self, client: &str) -> Vec<String> {
        let Some(m) = self
            .models
            .get(client)
            .or_else(|| self.models.get(&self.default_client))
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for opt in [&m.xs, &m.s, &m.m, &m.l, &m.xl] {
            if let Some(id) = opt {
                if !out.iter().any(|x| x == id) {
                    out.push(id.clone());
                }
            }
        }
        out
    }

    pub fn suggested_plan(&self, client: &str, tier: Tier) -> SuggestedPlan {
        SuggestedPlan {
            client: client.to_string(),
            model: self
                .model_for(client, tier)
                .unwrap_or("default")
                .to_string(),
            available_models: self.available_models(client),
            security: self.review.security_by_tier.get(tier),
            performance: self.review.performance_by_tier.get(tier),
            error_handling: self.review.error_handling_by_tier.get(tier),
            reviewers: self.agents.reviewers_by_tier.get(tier),
            evangelists: self.agents.evangelists_by_tier.get(tier),
            prompt_reviewers: self.agents.reviewers_by_tier.get(tier) > 0,
            prompt_evangelists: self.agents.evangelists_by_tier.get(tier) > 0,
        }
    }

    /// Suggested forge session knobs + which prompts to show.
    pub fn suggested_forge(&self, client: &str) -> SuggestedForge {
        let f = &self.forge;
        let model = f
            .model
            .clone()
            .or_else(|| {
                self.model_for(client, Tier::M)
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "default".into());
        SuggestedForge {
            client: client.to_string(),
            model: model.clone(),
            approach: f
                .approach
                .clone()
                .unwrap_or_else(|| f.default_approach.clone()),
            e2e: f.e2e,
            agents: f.agents.unwrap_or(f.default_agents),
            testers: f.testers.unwrap_or(f.default_testers),
            reviewers: f.reviewers.unwrap_or(f.default_reviewers),
            evangelists: f.evangelists.unwrap_or(f.default_evangelists),
            prompt_model: f.model.is_none(),
            prompt_approach: f.approach.is_none(),
            prompt_e2e: f.e2e.is_none(),
            prompt_agents: f.agents.is_none(),
            prompt_testers: f.testers.is_none(),
            prompt_reviewers: f.reviewers.is_none(),
            prompt_evangelists: f.evangelists.is_none(),
            enable_figma: f.enable_figma,
            enable_lore: f.enable_lore,
            enable_ticket_writeback: f.enable_ticket_writeback,
            enable_po: f.enable_po,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestedForge {
    pub client: String,
    pub model: String,
    pub approach: String,
    /// None means "prompt"; Some forces yes/no without prompt when config set.
    pub e2e: Option<bool>,
    pub agents: u32,
    pub testers: u32,
    pub reviewers: u32,
    pub evangelists: u32,
    pub prompt_model: bool,
    pub prompt_approach: bool,
    pub prompt_e2e: bool,
    pub prompt_agents: bool,
    pub prompt_testers: bool,
    pub prompt_reviewers: bool,
    pub prompt_evangelists: bool,
    pub enable_figma: bool,
    pub enable_lore: bool,
    pub enable_ticket_writeback: bool,
    pub enable_po: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestedPlan {
    pub client: String,
    /// Recommended model for this tier (default selection).
    pub model: String,
    /// All distinct models configured for this client — offer these in the model prompt.
    pub available_models: Vec<String>,
    pub security: bool,
    pub performance: bool,
    pub error_handling: bool,
    pub reviewers: u32,
    pub evangelists: u32,
    pub prompt_reviewers: bool,
    pub prompt_evangelists: bool,
}

pub fn config_dir() -> PathBuf {
    dirs_home().join(CONFIG_DIR_NAME)
}

pub fn config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE_NAME)
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Ensure `~/.scrutiny/config.toml` exists; copy from shipped default if missing.
pub fn ensure_config(shipped_default: &Path) -> Result<PathBuf> {
    let path = config_path();
    if path.exists() {
        return Ok(path);
    }
    fs::create_dir_all(config_dir()).context("create ~/.scrutiny")?;
    if shipped_default.exists() {
        fs::copy(shipped_default, &path).with_context(|| {
            format!(
                "copy default config from {} to {}",
                shipped_default.display(),
                path.display()
            )
        })?;
    } else {
        fs::write(&path, DEFAULT_TOML).context("write embedded default config")?;
    }
    Ok(path)
}

pub fn load_config(path: &Path) -> Result<Config> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).context("parse config.toml")?;
    Ok(cfg)
}

pub fn find_shipped_default(start: &Path) -> PathBuf {
    let mut cur = start.to_path_buf();
    for _ in 0..10 {
        let candidate = cur.join("config/default.toml");
        if candidate.exists() {
            return candidate;
        }
        if let Some(parent) = cur.parent() {
            cur = parent.to_path_buf();
        } else {
            break;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/default.toml")
}

pub const DEFAULT_TOML: &str = include_str!("../../../config/default.toml");

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_default_toml() {
        let cfg: Config = toml::from_str(DEFAULT_TOML).expect("parse default");
        assert_eq!(cfg.default_client, "claude");
        assert_eq!(cfg.agents.reviewers_by_tier.get(Tier::Xs), 0);
        assert!(!cfg.review.security_by_tier.get(Tier::S));
        assert!(cfg.review.security_by_tier.get(Tier::M));
        assert_eq!(cfg.pack.max_chars, 48_000);
        assert!(cfg.scan.enable);
        let claude = cfg.suggested_plan("claude", Tier::L);
        assert_eq!(claude.model, "claude-sonnet-4-6");
        let plan = cfg.suggested_plan("cursor", Tier::M);
        assert!(plan.prompt_reviewers);
        assert!(plan.prompt_evangelists);
        assert!(!plan.available_models.is_empty());
        assert!(plan.available_models.iter().any(|m| m == &plan.model));
        let plan_xs = cfg.suggested_plan("cursor", Tier::Xs);
        assert!(!plan_xs.prompt_reviewers);
        assert!(!plan_xs.prompt_evangelists);
        assert!(cfg.forge.enable_figma);
        assert_eq!(cfg.forge.default_approach, "tdd");
        let forge = cfg.suggested_forge("cursor");
        assert!(forge.prompt_approach);
        assert!(forge.prompt_e2e);
        assert_eq!(forge.approach, "tdd");
        assert_eq!(forge.agents, 2);
    }

    #[test]
    fn ensure_config_creates_file() {
        let dir = tempdir().unwrap();
        let old = std::env::var_os("HOME");
        std::env::set_var("HOME", dir.path());
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/default.toml");
        let path = ensure_config(&shipped).unwrap();
        assert!(path.exists());
        let _ = load_config(&path).unwrap();
        match old {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}
