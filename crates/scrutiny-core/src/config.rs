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
    /// Run spawned agents headless (stdout captured). When false, parley opens each
    /// agent in a visible terminal window in auto mode (claude + tmux/zellij/macOS).
    #[serde(default = "default_true")]
    pub headless: bool,
    /// Force headless client for `scrutiny probe` (cursor|claude|codex). Omit → detect + prompt.
    #[serde(default)]
    pub force_client: Option<String>,
    /// Force spawn mode: isolated | team. Omit → prompt (default **isolated**).
    #[serde(default)]
    pub force_spawn_mode: Option<String>,
    /// Editor for PR descriptions. Omit → `$VISUAL` → `$EDITOR` → `vi`.
    #[serde(default)]
    pub editor: Option<String>,
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
    #[serde(default)]
    pub parley: ParleyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParleyConfig {
    #[serde(default = "default_parley_members")]
    pub default_members: u32,
    #[serde(default = "default_parley_evangelists")]
    pub default_evangelists: u32,
    /// Verifiers that check fixes actually address comments (before evangelist).
    #[serde(default = "default_parley_verifiers")]
    pub default_verifiers: u32,
    /// Max push→fail→agent-fix→retry cycles after the initial push attempt.
    #[serde(default = "default_parley_push_fix_loops")]
    pub push_fix_max_loops: u32,
}

fn default_parley_members() -> u32 {
    1
}
fn default_parley_evangelists() -> u32 {
    1
}
fn default_parley_verifiers() -> u32 {
    1
}
fn default_parley_push_fix_loops() -> u32 {
    2
}

impl Default for ParleyConfig {
    fn default() -> Self {
        Self {
            default_members: default_parley_members(),
            default_evangelists: default_parley_evangelists(),
            default_verifiers: default_parley_verifiers(),
            push_fix_max_loops: default_parley_push_fix_loops(),
        }
    }
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
    /// Run the interactive branch step (create branch / +worktree / none).
    #[serde(default = "default_true")]
    pub enable_branch: bool,
    /// Headless branch behavior: "auto" (follow detection) | "never" (use current).
    #[serde(default = "default_branch_headless")]
    pub branch_headless: String,
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
fn default_branch_headless() -> String {
    "auto".into()
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
            enable_branch: true,
            branch_headless: default_branch_headless(),
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
    #[serde(default)]
    pub signals: ReviewSignalsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewSignalsConfig {
    #[serde(default)]
    pub ignore_content_signals: bool,
    #[serde(default = "default_security_path_globs")]
    pub security_path_globs: Vec<String>,
    #[serde(default = "default_security_diff_patterns")]
    pub security_diff_patterns: Vec<String>,
    #[serde(default = "default_performance_path_globs")]
    pub performance_path_globs: Vec<String>,
    #[serde(default = "default_performance_diff_patterns")]
    pub performance_diff_patterns: Vec<String>,
    #[serde(default = "default_performance_css_path_globs")]
    pub performance_css_path_globs: Vec<String>,
    #[serde(default = "default_performance_css_patterns")]
    pub performance_css_patterns: Vec<String>,
    #[serde(default = "default_error_handling_diff_patterns")]
    pub error_handling_diff_patterns: Vec<String>,
}

impl Default for ReviewSignalsConfig {
    fn default() -> Self {
        Self {
            ignore_content_signals: false,
            security_path_globs: default_security_path_globs(),
            security_diff_patterns: default_security_diff_patterns(),
            performance_path_globs: default_performance_path_globs(),
            performance_diff_patterns: default_performance_diff_patterns(),
            performance_css_path_globs: default_performance_css_path_globs(),
            performance_css_patterns: default_performance_css_patterns(),
            error_handling_diff_patterns: default_error_handling_diff_patterns(),
        }
    }
}

fn default_security_path_globs() -> Vec<String> {
    vec![
        "**/auth/**".into(),
        "**/*auth*".into(),
        "**/*oauth*".into(),
        "**/*session*".into(),
        "**/permission*/**".into(),
        "**/*permission*".into(),
        "**/*rbac*".into(),
        "**/*acl*".into(),
        "**/security/**".into(),
        "**/*crypto*".into(),
        "**/*secret*".into(),
        "**/*credential*".into(),
        "**/payment*/**".into(),
        "**/*billing*".into(),
        "**/*checkout*".into(),
        "**/middleware/**".into(),
        "**/api/**".into(),
        "**/server/**".into(),
        "**/backend/**".into(),
        "**/.env*".into(),
        "**/secrets/**".into(),
    ]
}

fn default_security_diff_patterns() -> Vec<String> {
    vec![
        r"(?i)\b(fetch|axios|XMLHttpRequest|got\(|node-fetch|reqwest|ureq|httpx|RestClient|HttpClient)\b".into(),
        r"(?i)\b(WebSocket|EventSource|graphql|apollo|trpc)\b".into(),
        r"(?i)\b(Authorization|Bearer\s+|JWT|csrf|xsrf|Set-Cookie|document\.cookie)\b".into(),
        r"(?i)\b(localStorage|sessionStorage|indexedDB)\.(get|set|remove)Item".into(),
        r"(?i)\b(password|passwd|api[_-]?key|private[_-]?key|client[_-]?secret)\b".into(),
        r"(?i)dangerouslySetInnerHTML|innerHTML\s*=|outerHTML\s*=".into(),
        r"(?i)\beval\s*\(|new\s+Function\s*\(".into(),
        r"(?i)\bexec\s*\(|child_process|Command::new|std::process::Command".into(),
        r"(?i)\b(Access-Control-Allow-Origin|\bcors\b|window\.location)".into(),
    ]
}

fn default_performance_path_globs() -> Vec<String> {
    vec![
        "**/hooks/**".into(),
        "**/domain/**".into(),
        "**/stores/**".into(),
        "**/data/**".into(),
        "**/workers/**".into(),
        "**/wasm/**".into(),
        "**/native/**".into(),
        "**/*List*".into(),
        "**/*Table*".into(),
        "**/*Grid*".into(),
        "**/*Virtual*".into(),
    ]
}

fn default_performance_diff_patterns() -> Vec<String> {
    vec![
        r"(?i)\b(useEffect|useLayoutEffect|useMemo|useCallback|useTransition|startTransition)\s*\(".into(),
        r"(?i)\b(React\.memo|memo\s*\()".into(),
        r"(?i)\.map\s*\(|\.filter\s*\(|\.reduce\s*\(|\.flatMap\s*\(".into(),
        r"(?i)\bfor\s*\(|\bwhile\s*\(|\.forEach\s*\(".into(),
        r"(?i)requestAnimationFrame|getBoundingClientRect|offsetWidth|offsetHeight|scrollTop".into(),
        r"(?i)\b(will-change|contain:|content-visibility:)".into(),
        r"(?i)\.clone\s*\(|to_vec\s*\(|collect::<Vec".into(),
        r"(?i)\bMutex::|\bRwLock::|blocking_".into(),
    ]
}

fn default_performance_css_path_globs() -> Vec<String> {
    vec![
        "**/*.css".into(),
        "**/*.scss".into(),
        "**/*.sass".into(),
        "**/*.less".into(),
    ]
}

fn default_performance_css_patterns() -> Vec<String> {
    vec![
        r"(?i):(nth-child|nth-of-type|has)\s*\(".into(),
        r"(?i)@keyframes|\banimation:".into(),
        r"(?i)[*]\s*[>+~]|[>+~]\s*[*]".into(),
        r"(?i)\bfilter:|\bbackdrop-filter:".into(),
    ]
}

fn default_error_handling_diff_patterns() -> Vec<String> {
    vec![
        r"(?i)\btry\s*\{|\bcatch\s*\(|\.catch\s*\(|finally\s*\{".into(),
        r"(?i)\basync\s+function|\basync\s*\(|await\s+".into(),
        r"(?i)\bResult<|anyhow::|thiserror|Promise\.reject".into(),
        r"(?i)\.unwrap\s*\(|\.expect\s*\(|panic!".into(),
        r"(?i)\b(onError|errorBoundary|ErrorBoundary|toast\.error)\b".into(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsConfig {
    pub reviewers_by_tier: TierCounts,
    pub evangelists_by_tier: TierCounts,
    #[serde(default = "default_max_agents_total")]
    pub max_agents_total: u32,
    #[serde(default = "default_max_reviewers_cap")]
    pub max_reviewers: u32,
    #[serde(default = "default_max_evangelists_cap")]
    pub max_evangelists: u32,
}

fn default_max_agents_total() -> u32 {
    4
}
fn default_max_reviewers_cap() -> u32 {
    2
}
fn default_max_evangelists_cap() -> u32 {
    1
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
    /// Per-file floor granted before any file gets extra symbol bodies.
    #[serde(default = "default_min_file_chars")]
    pub min_file_chars: usize,
    #[serde(default = "default_source_weight")]
    pub source_weight: u32,
    #[serde(default = "default_test_weight")]
    pub test_weight: u32,
    #[serde(default = "default_doc_weight")]
    pub doc_weight: u32,
    /// Cross-file referenced-signature resolution.
    #[serde(default = "default_true")]
    pub enable_xref: bool,
    #[serde(default = "default_xref_max_symbols")]
    pub xref_max_symbols: usize,
    #[serde(default = "default_xref_max_files_scanned")]
    pub xref_max_files_scanned: usize,
    #[serde(default = "default_xref_char_budget")]
    pub xref_char_budget: usize,
    #[serde(default = "default_xref_body_lines")]
    pub xref_body_lines: usize,
    #[serde(default = "default_annex_char_budget")]
    pub annex_char_budget: usize,
    #[serde(default)]
    pub explore: PackExploreConfig,
}

fn default_xref_body_lines() -> usize {
    40
}
fn default_annex_char_budget() -> usize {
    12_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackExploreConfig {
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default = "default_max_extra_reads")]
    pub max_extra_reads: u32,
    #[serde(default = "default_max_extra_chars")]
    pub max_extra_chars: usize,
    #[serde(default = "default_true")]
    pub prefer_read_over_bash: bool,
    #[serde(default)]
    pub allow_repo_grep: bool,
    #[serde(default = "default_true")]
    pub require_pack_path_hint: bool,
}

fn default_max_extra_reads() -> u32 {
    6
}
fn default_max_extra_chars() -> usize {
    24_000
}

impl Default for PackExploreConfig {
    fn default() -> Self {
        Self {
            enable: true,
            max_extra_reads: default_max_extra_reads(),
            max_extra_chars: default_max_extra_chars(),
            prefer_read_over_bash: true,
            allow_repo_grep: false,
            require_pack_path_hint: true,
        }
    }
}

fn default_min_file_chars() -> usize {
    1200
}
fn default_source_weight() -> u32 {
    4
}
fn default_test_weight() -> u32 {
    2
}
fn default_doc_weight() -> u32 {
    1
}
fn default_xref_max_symbols() -> usize {
    40
}
fn default_xref_max_files_scanned() -> usize {
    300
}
fn default_xref_char_budget() -> usize {
    6000
}

impl Default for PackConfig {
    fn default() -> Self {
        Self {
            max_chars: 48_000,
            doc_digest_lines: 40,
            symbol_context_lines: 3,
            min_file_chars: default_min_file_chars(),
            source_weight: default_source_weight(),
            test_weight: default_test_weight(),
            doc_weight: default_doc_weight(),
            enable_xref: true,
            xref_max_symbols: default_xref_max_symbols(),
            xref_max_files_scanned: default_xref_max_files_scanned(),
            xref_char_budget: default_xref_char_budget(),
            xref_body_lines: default_xref_body_lines(),
            annex_char_budget: default_annex_char_budget(),
            explore: PackExploreConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanConfig {
    pub enable: bool,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub i18n: ScanI18nConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanI18nConfig {
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default = "default_reference_locale")]
    pub reference_locale: String,
    #[serde(default = "default_i18n_path_globs")]
    pub path_globs: Vec<String>,
    #[serde(default = "default_true")]
    pub check_placeholders: bool,
    #[serde(default = "default_true")]
    pub check_empty_values: bool,
    #[serde(default)]
    pub full_catalog: bool,
}

fn default_reference_locale() -> String {
    "en".into()
}
fn default_i18n_path_globs() -> Vec<String> {
    vec![
        "**/i18n/locales/*.json".into(),
        "**/locales/*.json".into(),
        "**/locale/*.json".into(),
        "**/lang/*.json".into(),
        "**/translations/*.json".into(),
    ]
}

impl Default for ScanI18nConfig {
    fn default() -> Self {
        Self {
            enable: true,
            reference_locale: default_reference_locale(),
            path_globs: default_i18n_path_globs(),
            check_placeholders: true,
            check_empty_values: true,
            full_catalog: false,
        }
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            enable: true,
            commands: Vec::new(),
            i18n: ScanI18nConfig::default(),
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
        self.suggested_plan_with_signals(client, tier, &crate::signals::ContentSignals::default())
    }

    pub fn suggested_plan_with_signals(
        &self,
        client: &str,
        tier: Tier,
        content: &crate::signals::ContentSignals,
    ) -> SuggestedPlan {
        let tier_sec = self.review.security_by_tier.get(tier);
        let tier_perf = self.review.performance_by_tier.get(tier);
        let tier_err = self.review.error_handling_by_tier.get(tier);

        let security = if self.review.signals.ignore_content_signals {
            tier_sec
        } else {
            tier_sec && content.security
        };
        let performance = if self.review.signals.ignore_content_signals {
            tier_perf
        } else {
            tier_perf && content.performance
        };
        let error_handling = if self.review.signals.ignore_content_signals {
            tier_err
        } else {
            // On S+ tiers, allow error_handling when content hits OR when tier wants it and there is source
            tier_err && (content.error_handling || content.security || content.performance)
        };

        let mut reviewers = self.agents.reviewers_by_tier.get(tier).min(self.agents.max_reviewers);
        let mut evangelists = self
            .agents
            .evangelists_by_tier
            .get(tier)
            .min(self.agents.max_evangelists);

        // Soft total cap: reviewers + evangelists + specialists
        let specialists = (security as u32) + (performance as u32) + (error_handling as u32);
        let mut total = reviewers + evangelists + specialists;
        while total > self.agents.max_agents_total && evangelists > 0 {
            evangelists -= 1;
            total -= 1;
        }
        while total > self.agents.max_agents_total && reviewers > 1 {
            reviewers -= 1;
            total -= 1;
        }

        let security_reason = if !tier_sec {
            format!("tier {tier} default off")
        } else if security {
            content.security_reason.clone()
        } else if content.security_reason.is_empty() {
            "no security content signals".into()
        } else {
            content.security_reason.clone()
        };

        let performance_reason = if !tier_perf {
            format!("tier {tier} default off")
        } else if performance {
            content.performance_reason.clone()
        } else if content.performance_reason.is_empty() {
            "no performance content signals".into()
        } else {
            content.performance_reason.clone()
        };

        let error_handling_reason = if !tier_err {
            format!("tier {tier} default off")
        } else if error_handling {
            content.error_handling_reason.clone()
        } else if content.error_handling_reason.is_empty() {
            "no error-handling content signals".into()
        } else {
            content.error_handling_reason.clone()
        };

        SuggestedPlan {
            client: client.to_string(),
            model: self
                .model_for(client, tier)
                .unwrap_or("default")
                .to_string(),
            available_models: self.available_models(client),
            security,
            performance,
            error_handling,
            security_reason,
            performance_reason,
            error_handling_reason,
            reviewers,
            evangelists,
            prompt_reviewers: reviewers > 0,
            prompt_evangelists: evangelists > 0 || self.agents.evangelists_by_tier.get(tier) > 0,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    #[serde(default)]
    pub security_reason: String,
    #[serde(default)]
    pub performance_reason: String,
    #[serde(default)]
    pub error_handling_reason: String,
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
        assert!(!plan.prompt_evangelists); // M evangelists default 0
        assert!(!plan.available_models.is_empty());
        assert!(plan.available_models.iter().any(|m| m == &plan.model));
        let plan_xs = cfg.suggested_plan("cursor", Tier::Xs);
        assert!(!plan_xs.prompt_reviewers);
        assert!(!plan_xs.prompt_evangelists);
        assert_eq!(cfg.agents.max_agents_total, 4);
        assert!(cfg.scan.i18n.enable);
        assert!(cfg.pack.explore.enable);
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
