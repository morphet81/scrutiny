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
    pub models: BTreeMap<String, ClientModels>,
    pub review: ReviewConfig,
    pub agents: AgentsConfig,
    pub git: GitConfig,
    #[serde(default)]
    pub pack: PackConfig,
    #[serde(default)]
    pub scan: ScanConfig,
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

    pub fn suggested_plan(&self, client: &str, tier: Tier) -> SuggestedPlan {
        SuggestedPlan {
            client: client.to_string(),
            model: self
                .model_for(client, tier)
                .unwrap_or("default")
                .to_string(),
            security: self.review.security_by_tier.get(tier),
            performance: self.review.performance_by_tier.get(tier),
            error_handling: self.review.error_handling_by_tier.get(tier),
            reviewers: self.agents.reviewers_by_tier.get(tier),
            evangelists: self.agents.evangelists_by_tier.get(tier),
            prompt_reviewers: self.agents.reviewers_by_tier.get(tier) > 0,
            prompt_evangelists: self.agents.evangelists_by_tier.get(tier) > 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestedPlan {
    pub client: String,
    pub model: String,
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
        assert_eq!(cfg.default_client, "cursor");
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
        let plan_xs = cfg.suggested_plan("cursor", Tier::Xs);
        assert!(!plan_xs.prompt_reviewers);
        assert!(!plan_xs.prompt_evangelists);
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
