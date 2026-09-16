use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use crate::score::Tier;

fn tier_from_key(raw: &str) -> Option<Tier> {
    match raw.to_ascii_lowercase().as_str() {
        "xs" => Some(Tier::Xs),
        "s" => Some(Tier::S),
        "m" => Some(Tier::M),
        "l" => Some(Tier::L),
        "xl" => Some(Tier::Xl),
        _ => None,
    }
}

const CONFIG_DIR_NAME: &str = ".scrutiny";
const CONFIG_FILE_NAME: &str = "config.toml";
const LOCAL_CONFIG_FILE_NAME: &str = "scrutiny.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub default_client: String,
    #[serde(default = "default_true")]
    pub headless: bool,
    #[serde(default = "default_true")]
    pub caveman: bool,
    #[serde(default)]
    pub force_client: Option<String>,
    #[serde(default)]
    pub force_spawn_mode: Option<String>,
    #[serde(default)]
    pub editor: Option<String>,
    pub models: BTreeMap<String, ClientModels>,
    pub git: GitConfig,
    #[serde(default)]
    pub probe: ProbeConfig,
    #[serde(default)]
    pub forge: ForgeConfig,
    #[serde(default)]
    pub parley: ParleyConfig,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    #[serde(default)]
    pub prompts: PromptsConfig,
    /// Merged agent-model overrides: old [agent_models] + per-command [*.agent_models].
    /// Prefix catch-all: `parley = "l"` covers every `parley_*` role. Exact role wins.
    /// Special defaults: `parley_prepush_plan` → xs; `forge_loc_estimate` → m.
    #[serde(default)]
    pub agent_models: BTreeMap<String, String>,
}

/// Agent wall-clock limits. `agent_wall_secs` is the base every unset stage
/// derives from; each `*_wall_secs` overrides its stage alone.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimeoutsConfig {
    /// Base wall for one agent. Unset → 600. Every stage below with no explicit
    /// value derives from this (implement/fix ×2, non-headless ×3, forge item ×8).
    #[serde(default)]
    pub agent_wall_secs: Option<u64>,
    /// How often "still running" ticks print. Unset → 15.
    #[serde(default)]
    pub progress_secs: Option<u64>,
    /// Agents launched in a visible terminal window (they wait on a sentinel file).
    #[serde(default)]
    pub nonheadless_wall_secs: Option<u64>,
    #[serde(default)]
    pub probe_isolated_wall_secs: Option<u64>,
    #[serde(default)]
    pub probe_team_wall_secs: Option<u64>,
    #[serde(default)]
    pub probe_consolidate_wall_secs: Option<u64>,
    /// Triage "Ask a question…" agent.
    #[serde(default)]
    pub probe_ask_wall_secs: Option<u64>,
    /// Probe PR overview agent (shown before findings triage).
    #[serde(default)]
    pub probe_summary_wall_secs: Option<u64>,
    /// TDD test-plan agent.
    #[serde(default)]
    pub forge_test_plan_wall_secs: Option<u64>,
    #[serde(default)]
    pub forge_test_plan_revise_wall_secs: Option<u64>,
    /// Pre-implement LOC estimate agent (when `[forge] max_loc` is set).
    #[serde(default)]
    pub forge_loc_estimate_wall_secs: Option<u64>,
    /// PR-description agent.
    #[serde(default)]
    pub forge_pr_description_wall_secs: Option<u64>,
    #[serde(default)]
    pub forge_implement_wall_secs: Option<u64>,
    /// Verify-gate fix agent.
    #[serde(default)]
    pub forge_fix_wall_secs: Option<u64>,
    /// One multi-ticket forge item (worktree tab / headless child).
    #[serde(default)]
    pub forge_bulk_item_wall_secs: Option<u64>,
    /// Unset → falls back to `[parley] agent_wall_secs`.
    #[serde(default)]
    pub parley_agent_wall_secs: Option<u64>,
    /// Unset → falls back to `[parley] prepush_fix_wall_secs`.
    #[serde(default)]
    pub parley_prepush_fix_wall_secs: Option<u64>,
    /// Wall for the pre-push plan agent that splits the log into fix chunks.
    /// Unset → 120.
    #[serde(default)]
    pub parley_prepush_plan_wall_secs: Option<u64>,
    /// Kill a headless agent if it produces no stdout within this many seconds.
    /// Unset → 90. `0` disables the early kill (full wall only).
    #[serde(default)]
    pub headless_first_output_secs: Option<u64>,
}

/// Per-command timeout overrides for probe agents (all inherit [timeouts].agent_wall_secs).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ProbeTimeoutsConfig {
    #[serde(default)]
    pub agent_wall_secs: Option<u64>,
    #[serde(default)]
    pub isolated_wall_secs: Option<u64>,
    #[serde(default)]
    pub team_wall_secs: Option<u64>,
    #[serde(default)]
    pub consolidate_wall_secs: Option<u64>,
    #[serde(default)]
    pub summary_wall_secs: Option<u64>,
    #[serde(default)]
    pub ask_wall_secs: Option<u64>,
}

/// Per-command timeout overrides for forge agents (all inherit [timeouts].agent_wall_secs).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ForgeTimeoutsConfig {
    #[serde(default)]
    pub implement_wall_secs: Option<u64>,
    #[serde(default)]
    pub fix_wall_secs: Option<u64>,
    #[serde(default)]
    pub test_plan_wall_secs: Option<u64>,
    #[serde(default)]
    pub test_plan_revise_wall_secs: Option<u64>,
    #[serde(default)]
    pub loc_estimate_wall_secs: Option<u64>,
    #[serde(default)]
    pub pr_description_wall_secs: Option<u64>,
    #[serde(default)]
    pub item_wall_secs: Option<u64>,
}

/// Per-command timeout overrides for parley agents (all inherit [timeouts].agent_wall_secs).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ParleyTimeoutsConfig {
    #[serde(default)]
    pub agent_wall_secs: Option<u64>,
    #[serde(default)]
    pub prepush_fix_wall_secs: Option<u64>,
    #[serde(default)]
    pub prepush_plan_wall_secs: Option<u64>,
}

/// Probe command config: review/agent/pack/scan settings + probe-specific overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeConfig {
    /// Headless PR overview agent before findings triage (`scrutiny probe`).
    #[serde(default = "default_true")]
    pub pr_summary: bool,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub pack: PackConfig,
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default)]
    pub timeouts: ProbeTimeoutsConfig,
    #[serde(default)]
    pub agent_models: BTreeMap<String, String>,
    #[serde(default)]
    pub prompts: BTreeMap<String, String>,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            pr_summary: true,
            review: ReviewConfig::default(),
            agents: AgentsConfig::default(),
            pack: PackConfig::default(),
            scan: ScanConfig::default(),
            timeouts: ProbeTimeoutsConfig::default(),
            agent_models: BTreeMap::new(),
            prompts: BTreeMap::new(),
        }
    }
}

/// User-injected prompt text prepended to spawned-agent prompts.
/// `global` goes to every agent; `agents[<role>]` targets one role. Order:
/// global → agent → scrutiny's own prompt. Role key = agent label prefix with
/// `-` replaced by `_` (e.g. `parley-member` → `parley_member`); unknown keys ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptsConfig {
    #[serde(default)]
    pub global: String,
    #[serde(default)]
    pub agents: BTreeMap<String, String>,
}

impl PromptsConfig {
    /// Combined prefix for an agent type: global then per-agent override,
    /// each trimmed, joined by a blank line. Empty when neither is set.
    pub fn prefix_for(&self, agent_type: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        let g = self.global.trim();
        if !g.is_empty() {
            parts.push(g);
        }
        let a = self.agents.get(agent_type).map(|s| s.trim()).unwrap_or("");
        if !a.is_empty() {
            parts.push(a);
        }
        parts.join("\n\n")
    }
}

static PROMPT_OVERRIDES: OnceLock<RwLock<PromptsConfig>> = OnceLock::new();

fn prompt_overrides() -> &'static RwLock<PromptsConfig> {
    PROMPT_OVERRIDES.get_or_init(|| RwLock::new(PromptsConfig::default()))
}

fn store_prompt_overrides(p: &PromptsConfig) {
    if let Ok(mut w) = prompt_overrides().write() {
        *w = p.clone();
    }
}

/// Resolve the injected prompt prefix for an agent type from the process-global
/// overrides set by the most recent [`load_config`]. Empty if unset.
pub fn resolve_prompt_prefix(agent_type: &str) -> String {
    prompt_overrides()
        .read()
        .map(|p| p.prefix_for(agent_type))
        .unwrap_or_default()
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
    /// Deprecated — retained for back-compat deserialization. See `prepush_fix_max_loops`.
    #[serde(default = "default_parley_push_fix_loops")]
    pub push_fix_max_loops: u32,
    /// Override command for the quiet pre-push gate. Empty → run the repo's
    /// actual pre-push hook (`git hook run pre-push`).
    #[serde(default)]
    pub prepush_cmd: Option<String>,
    /// Max scrutiny-runs-checks → fix-agent → re-check cycles in the pre-push gate.
    #[serde(default = "default_prepush_fix_loops")]
    pub prepush_fix_max_loops: u32,
    /// Cap on fix chunks the pre-push plan agent may emit (default 8).
    #[serde(default = "default_prepush_fix_max_chunks")]
    pub prepush_fix_max_chunks: u32,
    /// Wall-clock seconds for each pre-push fix agent in the gate. Superseded by
    /// `[timeouts] parley_prepush_fix_wall_secs`; kept for back-compat.
    #[serde(default)]
    pub prepush_fix_wall_secs: Option<u64>,
    /// Wall-clock seconds for each member / verifier / evangelist agent.
    /// Superseded by `[timeouts] parley_agent_wall_secs`; kept for back-compat.
    #[serde(default)]
    pub agent_wall_secs: Option<u64>,
    /// Run a repair pass that re-implements threads left as stubs or rejected by
    /// a verifier, so failures never post as PR replies.
    #[serde(default = "default_parley_repair")]
    pub repair: bool,
    /// Pass `--no-verify` to `git push` in `scrutiny parley`. Skips pre-push
    /// hooks. Default false.
    #[serde(default)]
    pub push_no_verify: bool,
    #[serde(default)]
    pub timeouts: ParleyTimeoutsConfig,
    #[serde(default)]
    pub agent_models: BTreeMap<String, String>,
    #[serde(default)]
    pub prompts: BTreeMap<String, String>,
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
fn default_prepush_fix_loops() -> u32 {
    5
}
fn default_prepush_fix_max_chunks() -> u32 {
    8
}
fn default_parley_repair() -> bool {
    true
}

impl Default for ParleyConfig {
    fn default() -> Self {
        Self {
            default_members: default_parley_members(),
            default_evangelists: default_parley_evangelists(),
            default_verifiers: default_parley_verifiers(),
            push_fix_max_loops: default_parley_push_fix_loops(),
            prepush_cmd: None,
            prepush_fix_max_loops: default_prepush_fix_loops(),
            prepush_fix_max_chunks: default_prepush_fix_max_chunks(),
            prepush_fix_wall_secs: None,
            agent_wall_secs: None,
            repair: default_parley_repair(),
            push_no_verify: false,
            timeouts: ParleyTimeoutsConfig::default(),
            agent_models: BTreeMap::new(),
            prompts: BTreeMap::new(),
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
    #[serde(default, alias = "model")]
    pub force_model: Option<String>,
    #[serde(default)]
    pub all: ForgeAllConfig,
    #[serde(default)]
    pub timeouts: ForgeTimeoutsConfig,
    #[serde(default)]
    pub agent_models: BTreeMap<String, String>,
    #[serde(default)]
    pub prompts: BTreeMap<String, String>,
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
    /// Override command for the pre-push checks appended to the verify gate.
    /// Empty → run the repo's actual pre-push hook (`git hook run pre-push`).
    #[serde(default)]
    pub prepush_cmd: Option<String>,
    /// Gate on coverage % when measurable (auto-derived commands only).
    #[serde(default = "default_true")]
    pub verify_coverage: bool,
    /// TEMPORARY: skip the verify gate (tests / lint / pre-push) after implement.
    /// Set `false` to restore. Default true until ship pipeline is re-enabled.
    #[serde(default = "default_true")]
    pub skip_verify: bool,
    /// TEMPORARY: skip commit + draft PR after implement. Use `scrutiny pr` after.
    /// Set `false` to restore. Default true until ship pipeline is re-enabled.
    #[serde(default = "default_true")]
    pub skip_ship: bool,
    /// Run the interactive branch step (create branch / +worktree / none).
    #[serde(default = "default_true")]
    pub enable_branch: bool,
    /// Headless branch behavior: "auto" (follow detection) | "never" (use current).
    #[serde(default = "default_branch_headless")]
    pub branch_headless: String,
    /// Optional. When set, a dedicated headless agent writes the PR description
    /// from this prompt + the diff, overriding the implement agent's pr_body.
    #[serde(default)]
    pub pr_description_prompt: Option<String>,
    /// Optional. When set, forge runs an `m`-tier LOC estimate agent before
    /// implement and gates if the estimated PR add+del LOC exceeds this budget.
    #[serde(default)]
    pub max_loc: Option<u32>,
    /// Exclude test paths from LOC counting / estimate (default true).
    #[serde(default = "default_true")]
    pub loc_exclude_test: bool,
    /// Exclude doc paths from LOC counting / estimate (default true).
    #[serde(default = "default_true")]
    pub loc_exclude_doc: bool,
    /// Exclude comment-only lines from LOC counting / estimate (default true).
    #[serde(default = "default_true")]
    pub loc_exclude_comments: bool,
    /// File extensions (no leading dot) excluded from LOC counting / estimate.
    #[serde(default = "default_loc_exclude_extensions")]
    pub loc_exclude_extensions: Vec<String>,
    #[serde(default)]
    pub complexity: ComplexityConfig,
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
    5
}
fn default_branch_headless() -> String {
    "auto".into()
}

/// Common binary / opaque extensions excluded from forge LOC counting by default.
pub fn default_loc_exclude_extensions() -> Vec<String> {
    [
        "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "pdf", "zip", "tar", "gz", "tgz", "bz2",
        "xz", "7z", "rar", "woff", "woff2", "ttf", "otf", "eot", "mp3", "mp4", "mov", "avi",
        "webm", "wav", "wasm", "dll", "so", "dylib", "exe", "bin", "class", "jar", "war", "pyc",
        "pyo", "o", "a", "obj", "db", "sqlite", "sqlite3",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
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
            force_model: None,
            all: ForgeAllConfig::default(),
            timeouts: ForgeTimeoutsConfig::default(),
            agent_models: BTreeMap::new(),
            prompts: BTreeMap::new(),
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
            prepush_cmd: None,
            verify_coverage: true,
            skip_verify: true,
            skip_ship: true,
            enable_branch: true,
            branch_headless: default_branch_headless(),
            pr_description_prompt: None,
            max_loc: None,
            loc_exclude_test: true,
            loc_exclude_doc: true,
            loc_exclude_comments: true,
            loc_exclude_extensions: default_loc_exclude_extensions(),
            complexity: ComplexityConfig::default(),
        }
    }
}

/// Defaults for multi-ticket `scrutiny forge` (Jira URLs → assign / progress / worktree / implement).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeAllConfig {
    /// Prefix for new branches (`feat-nero-123`).
    #[serde(
        default = "default_forge_all_branch_prefix",
        alias = "branch-prefix",
        alias = "branch-preffix",
        alias = "branch_preffix"
    )]
    pub branch_prefix: String,
    /// Directory that will hold each worktree folder (absolute, or relative to repo root).
    #[serde(
        default = "default_forge_all_worktree_parent",
        alias = "worktree-parent-folder",
        alias = "worktree_parent_folder"
    )]
    pub worktree_parent_folder: String,
    /// Default answer to the TDD knob.
    #[serde(default = "default_true", alias = "use-tdd")]
    pub use_tdd: bool,
    /// Default coverage % (forge `coverage_pct`).
    #[serde(default = "default_forge_all_coverage", alias = "test-coverage")]
    pub test_coverage: u32,
    /// Default answer to the e2e knob.
    #[serde(
        default = "default_true",
        alias = "require-e2e",
        alias = "require_e2",
        alias = "require-e2"
    )]
    pub require_e2e: bool,
    /// How many implement agents (forge `agents` / team size).
    #[serde(default = "default_agents_2", alias = "team-size")]
    pub team_size: u32,
    /// `single` | `team`.
    #[serde(default = "default_forge_all_spawn", alias = "spawn-mode")]
    pub spawn_mode: String,
    /// Agent CLI: `claude` | `cursor` | `codex`.
    #[serde(default = "default_forge_all_cli", alias = "agent-cli")]
    pub agent_cli: String,
    /// Model id / tier name for forge.
    #[serde(default = "default_forge_all_model")]
    pub model: String,
    /// Jira assignee (`@me`, email, or account id).
    #[serde(default = "default_forge_all_assignee", alias = "jira-assignee")]
    pub jira_assignee: String,
    /// Status name for `acli jira workitem transition` (default `In Progress`).
    #[serde(
        default = "default_forge_all_in_progress",
        alias = "in-progress-status"
    )]
    pub in_progress_status: String,
    /// Shell commands run in each worktree (cwd = worktree) before `scrutiny forge`.
    #[serde(default, alias = "init-commands")]
    pub init_commands: Vec<String>,
}

fn default_forge_all_branch_prefix() -> String {
    "feat".into()
}
fn default_forge_all_worktree_parent() -> String {
    "..".into()
}
fn default_forge_all_coverage() -> u32 {
    100
}
fn default_forge_all_spawn() -> String {
    "single".into()
}
fn default_forge_all_cli() -> String {
    "claude".into()
}
fn default_forge_all_model() -> String {
    "sonnet".into()
}
fn default_forge_all_assignee() -> String {
    "@me".into()
}
fn default_forge_all_in_progress() -> String {
    "In Progress".into()
}

impl Default for ForgeAllConfig {
    fn default() -> Self {
        Self {
            branch_prefix: default_forge_all_branch_prefix(),
            worktree_parent_folder: default_forge_all_worktree_parent(),
            use_tdd: true,
            test_coverage: default_forge_all_coverage(),
            require_e2e: true,
            team_size: default_agents_2(),
            spawn_mode: default_forge_all_spawn(),
            agent_cli: default_forge_all_cli(),
            model: default_forge_all_model(),
            jira_assignee: default_forge_all_assignee(),
            in_progress_status: default_forge_all_in_progress(),
            init_commands: Vec::new(),
        }
    }
}

/// Ticket complexity scoring configuration for `forge`.
/// Controls keyword lists, story-point field names, label bumps/lowers, and tier thresholds.
/// All fields have sensible defaults — omitting `[forge.complexity]` entirely is valid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplexityConfig {
    /// Jira custom field names (or standard names) that hold story-point estimates.
    #[serde(default = "default_story_point_fields")]
    pub story_point_fields: Vec<String>,
    /// Keywords suggesting broad, cross-cutting scope (each hit adds to the score).
    #[serde(default = "default_breadth_keywords")]
    pub breadth_keywords: Vec<String>,
    /// Keywords suggesting external / API integration work.
    #[serde(default = "default_integration_keywords")]
    pub integration_keywords: Vec<String>,
    /// Keywords suggesting security-sensitive or high-risk changes.
    #[serde(default = "default_risk_keywords")]
    pub risk_keywords: Vec<String>,
    /// Keywords that indicate trivial changes (reduce the score).
    #[serde(default = "default_trivial_keywords")]
    pub trivial_keywords: Vec<String>,
    /// Label substrings that push the tier up by one step.
    #[serde(default = "default_bump_labels")]
    pub bump_labels: Vec<String>,
    /// Label substrings that pull the tier down by one step.
    #[serde(default = "default_lower_labels")]
    pub lower_labels: Vec<String>,
    /// Inclusive upper bounds for tiers [XS, S, M, L]; anything above → XL.
    /// Matches the same scale used by `score.rs` for code-diff scoring.
    #[serde(default = "default_tier_thresholds")]
    pub tier_thresholds: [u32; 4],
}

fn default_story_point_fields() -> Vec<String> {
    vec![
        "story_points".into(),
        "customfield_10016".into(),
        "customfield_10028".into(),
        "customfield_10034".into(),
    ]
}
fn default_breadth_keywords() -> Vec<String> {
    vec![
        "refactor".into(),
        "migrate".into(),
        "rewrite".into(),
        "redesign".into(),
        "architecture".into(),
        "overhaul".into(),
        "restructure".into(),
        "across".into(),
    ]
}
fn default_integration_keywords() -> Vec<String> {
    vec![
        "api".into(),
        "endpoint".into(),
        "webhook".into(),
        "schema".into(),
        "database".into(),
        "migration".into(),
        "third-party".into(),
        "external".into(),
        "integration".into(),
    ]
}
fn default_risk_keywords() -> Vec<String> {
    vec![
        "auth".into(),
        "security".into(),
        "payment".into(),
        "permission".into(),
        "encryption".into(),
        "pii".into(),
        "credential".into(),
        "oauth".into(),
        "token".into(),
    ]
}
fn default_trivial_keywords() -> Vec<String> {
    vec![
        "typo".into(),
        "copy".into(),
        "wording".into(),
        "rename".into(),
        "bump".into(),
        "documentation".into(),
        "translation".into(),
        "minor".into(),
        "spelling".into(),
    ]
}
fn default_bump_labels() -> Vec<String> {
    vec![
        "urgent".into(),
        "complex".into(),
        "breaking-change".into(),
        "breaking".into(),
        "epic".into(),
        "large".into(),
    ]
}
fn default_lower_labels() -> Vec<String> {
    vec![
        "trivial".into(),
        "minor".into(),
        "quick".into(),
        "simple".into(),
        "easy".into(),
        "small".into(),
    ]
}
fn default_tier_thresholds() -> [u32; 4] {
    [18, 35, 55, 95]
}

impl Default for ComplexityConfig {
    fn default() -> Self {
        Self {
            story_point_fields: default_story_point_fields(),
            breadth_keywords: default_breadth_keywords(),
            integration_keywords: default_integration_keywords(),
            risk_keywords: default_risk_keywords(),
            trivial_keywords: default_trivial_keywords(),
            bump_labels: default_bump_labels(),
            lower_labels: default_lower_labels(),
            tier_thresholds: default_tier_thresholds(),
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
        r"(?i)\b(useEffect|useLayoutEffect|useMemo|useCallback|useTransition|startTransition)\s*\("
            .into(),
        r"(?i)\b(React\.memo|memo\s*\()".into(),
        r"(?i)\.map\s*\(|\.filter\s*\(|\.reduce\s*\(|\.flatMap\s*\(".into(),
        r"(?i)\bfor\s*\(|\bwhile\s*\(|\.forEach\s*\(".into(),
        r"(?i)requestAnimationFrame|getBoundingClientRect|offsetWidth|offsetHeight|scrollTop"
            .into(),
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
    /// Build/test artifact globs never staged by parley/forge commits (they are
    /// cleaned from the working tree instead). Guards against agent-created
    /// coverage dirs and similar leftovers that are not gitignored.
    #[serde(default = "default_artifact_globs")]
    pub artifact_globs: Vec<String>,
    /// Pass `--no-verify` to `git push` in `scrutiny pr` and `scrutiny forge`.
    /// Skips pre-push hooks. Default false.
    #[serde(default)]
    pub push_no_verify: bool,
}

fn default_artifact_globs() -> Vec<String> {
    [
        "coverage-*/*",
        "coverage/*",
        ".nyc_output/*",
        ".vitest/*",
        "playwright-report/*",
        "test-results/*",
        "*.tsbuildinfo",
        "*.log",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
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
    /// Filter missing-key warnings for unsupported plural categories.
    /// When true, only warn about missing keys when the target locale
    /// supports that plural category. Unknown locales remain conservative (warn).
    #[serde(default = "default_true")]
    pub plural_aware_filtering: bool,
    /// Explicit locale → supported plural categories override map.
    /// Example: `{"ms": ["other"], "th": ["other"]}` for locales with only `other`.
    /// Built-in table covers common single-category locales when omitted.
    #[serde(default)]
    pub locale_plural_categories: std::collections::BTreeMap<String, Vec<String>>,
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
            plural_aware_filtering: true,
            locale_plural_categories: std::collections::BTreeMap::new(),
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

impl Default for TierBools {
    fn default() -> Self {
        Self { xs: false, s: false, m: false, l: false, xl: false }
    }
}

impl Default for TierCounts {
    fn default() -> Self {
        Self { xs: 0, s: 0, m: 0, l: 0, xl: 0 }
    }
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            security_by_tier: TierBools { xs: false, s: false, m: true, l: true, xl: true },
            performance_by_tier: TierBools { xs: false, s: false, m: false, l: true, xl: true },
            error_handling_by_tier: TierBools { xs: false, s: true, m: true, l: true, xl: true },
            signals: ReviewSignalsConfig::default(),
        }
    }
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            reviewers_by_tier: TierCounts { xs: 0, s: 1, m: 1, l: 2, xl: 2 },
            evangelists_by_tier: TierCounts { xs: 0, s: 0, m: 0, l: 1, xl: 1 },
            max_agents_total: default_max_agents_total(),
            max_reviewers: default_max_reviewers_cap(),
            max_evangelists: default_max_evangelists_cap(),
        }
    }
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

    /// Resolve the model for an agent role.
    ///
    /// - Explicit `[agent_models.<role>]` wins (tier name or raw model id).
    /// - Else prefix catch-all: `parley_member` reads `[agent_models.parley]`.
    /// - `parley_prepush_plan` with neither entry defaults to tier `xs`.
    /// - `forge_loc_estimate` with neither entry defaults to tier `m`.
    /// - Otherwise → `session_model`.
    pub fn resolve_agent_model(&self, client: &str, role: &str, session_model: &str) -> String {
        let lookup = |key: &str| {
            self.agent_models
                .get(key)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
        };
        let family = role.split_once('_').map(|(p, _)| p);
        let raw = match lookup(role).or_else(|| family.and_then(lookup)) {
            Some(v) => v,
            None if role == "parley_prepush_plan" => "xs",
            None if role == "forge_loc_estimate" => "m",
            None => return session_model.to_string(),
        };
        if let Some(tier) = tier_from_key(raw) {
            return self
                .model_for(client, tier)
                .unwrap_or(session_model)
                .to_string();
        }
        raw.to_string()
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
        let tier_sec = self.probe.review.security_by_tier.get(tier);
        let tier_perf = self.probe.review.performance_by_tier.get(tier);
        let tier_err = self.probe.review.error_handling_by_tier.get(tier);

        let security = if self.probe.review.signals.ignore_content_signals {
            tier_sec
        } else {
            tier_sec && content.security
        };
        let performance = if self.probe.review.signals.ignore_content_signals {
            tier_perf
        } else {
            tier_perf && content.performance
        };
        let error_handling = if self.probe.review.signals.ignore_content_signals {
            tier_err
        } else {
            // On S+ tiers, allow error_handling when content hits OR when tier wants it and there is source
            tier_err && (content.error_handling || content.security || content.performance)
        };

        let mut reviewers = self
            .probe.agents
            .reviewers_by_tier
            .get(tier)
            .min(self.probe.agents.max_reviewers);
        let mut evangelists = self
            .probe.agents
            .evangelists_by_tier
            .get(tier)
            .min(self.probe.agents.max_evangelists);

        // Soft total cap: reviewers + evangelists + specialists
        let specialists = (security as u32) + (performance as u32) + (error_handling as u32);
        let mut total = reviewers + evangelists + specialists;
        while total > self.probe.agents.max_agents_total && evangelists > 0 {
            evangelists -= 1;
            total -= 1;
        }
        while total > self.probe.agents.max_agents_total && reviewers > 1 {
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
            prompt_evangelists: evangelists > 0 || self.probe.agents.evangelists_by_tier.get(tier) > 0,
        }
    }

    /// Suggested forge session knobs + which prompts to show.
    ///
    /// `tier`, `complexity_score`, `complexity_reason` come from
    /// `forge::complexity::estimate_ticket_tier` — computed by the caller
    /// (fetch.rs) after the ticket is fully built so figma_urls are available.
    /// Pass `(Tier::M, 0, String::new())` when no ticket is available (e.g. tests).
    pub fn suggested_forge(
        &self,
        client: &str,
        tier: Tier,
        complexity_score: u32,
        complexity_reason: String,
    ) -> SuggestedForge {
        let f = &self.forge;
        let model = f
            .force_model
            .clone()
            .or_else(|| self.model_for(client, tier).map(|s| s.to_string()))
            .unwrap_or_else(|| "default".into());
        SuggestedForge {
            client: client.to_string(),
            model,
            tier,
            complexity_score,
            complexity_reason,
            available_models: self.available_models(client),
            approach: f
                .approach
                .clone()
                .unwrap_or_else(|| f.default_approach.clone()),
            e2e: f.e2e,
            agents: f.agents.unwrap_or(f.default_agents),
            testers: f.testers.unwrap_or(f.default_testers),
            reviewers: f.reviewers.unwrap_or(f.default_reviewers),
            evangelists: f.evangelists.unwrap_or(f.default_evangelists),
            prompt_model: f.force_model.is_none(),
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
    /// Ticket-derived complexity tier that produced the default model.
    #[serde(default)]
    pub tier: Tier,
    /// Raw complexity score (0–100) from the ticket estimator.
    #[serde(default)]
    pub complexity_score: u32,
    /// Human-readable summary of the top signals that drove the tier estimate.
    #[serde(default)]
    pub complexity_reason: String,
    /// All distinct model ids configured for this client (xs→xl order, deduped).
    #[serde(default)]
    pub available_models: Vec<String>,
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

/// Find a project-local `scrutiny.toml` by walking up from cwd to the repo root.
fn local_config_path() -> Option<PathBuf> {
    let mut cur = std::env::current_dir().ok()?;
    loop {
        let candidate = cur.join(LOCAL_CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if cur.join(".git").exists() {
            return None;
        }
        cur = cur.parent()?.to_path_buf();
    }
}

/// Recursively merge `over` into `base`. Tables merge key-by-key; scalars and
/// arrays are replaced wholesale by the override.
fn merge_toml(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(bv) => merge_toml(bv, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

pub fn load_config(path: &Path) -> Result<Config> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let mut value: toml::Value = toml::from_str(&text).context("parse config.toml")?;

    if let Some(local) = local_config_path() {
        let ltext =
            fs::read_to_string(&local).with_context(|| format!("read {}", local.display()))?;
        let lvalue: toml::Value =
            toml::from_str(&ltext).with_context(|| format!("parse {}", local.display()))?;
        merge_toml(&mut value, lvalue);
    }

    normalize_value(&mut value);
    let mut cfg: Config = value.try_into().context("parse config.toml")?;
    store_prompt_overrides(&cfg.prompts);
    crate::caveman::store_caveman_enabled(cfg.caveman);
    merge_command_timeouts(&mut cfg);
    merge_agent_models(&mut cfg);
    crate::timeouts::install(crate::timeouts::Timeouts::resolve(&cfg.timeouts));
    Ok(cfg)
}

/// Migrate old top-level keys to the new nested structure for backward compat.
fn normalize_value(v: &mut toml::Value) {
    let toml::Value::Table(root) = v else { return };
    for (old_key, new_parent, new_child) in [
        ("review", "probe", "review"),
        ("agents", "probe", "agents"),
        ("pack", "probe", "pack"),
        ("scan", "probe", "scan"),
        ("forge_all", "forge", "all"),
    ] {
        if let Some(val) = root.remove(old_key) {
            let parent = root
                .entry(new_parent.to_string())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            if let toml::Value::Table(pt) = parent {
                if let Some(existing) = pt.get_mut(new_child) {
                    merge_toml(existing, val);
                } else {
                    pt.insert(new_child.to_string(), val);
                }
            }
        }
    }
    // Lift legacy `pr_summary` off `probe.review` onto `probe` (new home).
    if let Some(toml::Value::Table(probe)) = root.get_mut("probe") {
        if let Some(toml::Value::Table(review)) = probe.get_mut("review") {
            if let Some(ps) = review.remove("pr_summary") {
                probe.entry("pr_summary".to_string()).or_insert(ps);
            }
        }
    }
}

/// Copy per-command timeout fields into the flat TimeoutsConfig.
/// Per-command wins over flat when both are set; flat explicit value always wins over
/// nothing. Order: flat `[timeouts]` explicit > per-command > nothing.
fn merge_command_timeouts(cfg: &mut Config) {
    macro_rules! fill {
        ($flat:ident, $src:expr) => {
            if cfg.timeouts.$flat.is_none() {
                cfg.timeouts.$flat = $src;
            }
        };
    }
    // Probe
    let pt = cfg.probe.timeouts;
    fill!(probe_isolated_wall_secs, pt.isolated_wall_secs);
    fill!(probe_team_wall_secs, pt.team_wall_secs);
    fill!(probe_consolidate_wall_secs, pt.consolidate_wall_secs);
    fill!(probe_summary_wall_secs, pt.summary_wall_secs);
    fill!(probe_ask_wall_secs, pt.ask_wall_secs);

    // Forge
    let ft = cfg.forge.timeouts;
    fill!(forge_implement_wall_secs, ft.implement_wall_secs);
    fill!(forge_fix_wall_secs, ft.fix_wall_secs);
    fill!(forge_test_plan_wall_secs, ft.test_plan_wall_secs);
    fill!(forge_test_plan_revise_wall_secs, ft.test_plan_revise_wall_secs);
    fill!(forge_loc_estimate_wall_secs, ft.loc_estimate_wall_secs);
    fill!(forge_pr_description_wall_secs, ft.pr_description_wall_secs);
    fill!(forge_bulk_item_wall_secs, ft.item_wall_secs);

    // Parley — per-command struct wins; legacy [parley] direct fields are fallback
    let parley_t = cfg.parley.timeouts;
    fill!(parley_agent_wall_secs, parley_t.agent_wall_secs.or(cfg.parley.agent_wall_secs));
    fill!(parley_prepush_fix_wall_secs, parley_t.prepush_fix_wall_secs.or(cfg.parley.prepush_fix_wall_secs));
    fill!(parley_prepush_plan_wall_secs, parley_t.prepush_plan_wall_secs);
}

/// Expand per-command agent_models maps into the flat agent_models BTreeMap.
/// Forge/parley keys are prefixed (`forge_implement`, `parley_member`, etc.).
/// Probe keys stay bare (`reviewer`, `summary`) to match agent labels.
/// A `default` key in a per-command map acts as the catch-all (e.g. `parley`).
fn merge_agent_models(cfg: &mut Config) {
    let mut flat = std::mem::take(&mut cfg.agent_models);

    for (prefix, map) in [
        ("probe", &cfg.probe.agent_models),
        ("forge", &cfg.forge.agent_models),
        ("parley", &cfg.parley.agent_models),
    ] {
        for (k, v) in map {
            let flat_key = if k == "default" {
                prefix.to_string()
            } else if prefix == "probe" {
                k.clone()
            } else {
                format!("{prefix}_{k}")
            };
            flat.entry(flat_key).or_insert_with(|| v.clone());
        }
    }

    cfg.agent_models = flat;
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
        assert!(cfg.caveman);
        assert!(cfg.probe.pr_summary);
        assert_eq!(cfg.probe.agents.reviewers_by_tier.get(Tier::Xs), 0);
        assert!(!cfg.probe.review.security_by_tier.get(Tier::S));
        assert!(cfg.probe.review.security_by_tier.get(Tier::M));
        assert_eq!(cfg.probe.pack.max_chars, 48_000);
        assert!(cfg.probe.scan.enable);
        let claude = cfg.suggested_plan("claude", Tier::L);
        assert_eq!(claude.model, "opus");
        let plan = cfg.suggested_plan("cursor", Tier::M);
        assert!(plan.prompt_reviewers);
        assert!(!plan.prompt_evangelists); // M evangelists default 0
        assert!(!plan.available_models.is_empty());
        assert!(plan.available_models.iter().any(|m| m == &plan.model));
        let plan_xs = cfg.suggested_plan("cursor", Tier::Xs);
        assert!(!plan_xs.prompt_reviewers);
        assert!(!plan_xs.prompt_evangelists);
        assert_eq!(cfg.probe.agents.max_agents_total, 4);
        assert!(cfg.probe.scan.i18n.enable);
        assert!(cfg.probe.pack.explore.enable);
        assert!(cfg.forge.enable_figma);
        assert_eq!(cfg.forge.default_approach, "tdd");
        assert!(cfg.forge.max_loc.is_none());
        assert!(cfg.forge.loc_exclude_test);
        assert!(cfg.forge.loc_exclude_doc);
        assert!(cfg.forge.loc_exclude_comments);
        assert!(cfg.forge.loc_exclude_extensions.iter().any(|e| e == "png"));
        let forge = cfg.suggested_forge("cursor", Tier::M, 0, String::new());
        assert!(forge.prompt_approach);
        assert!(forge.prompt_e2e);
        assert_eq!(forge.approach, "tdd");
        assert_eq!(forge.agents, 2);
        assert!(!forge.available_models.is_empty());
        assert_eq!(forge.tier, Tier::M);
        assert!(cfg.prompts.global.is_empty());
        assert!(cfg.prompts.agents.is_empty());
        assert!(cfg.agent_models.is_empty());
        assert_eq!(
            cfg.resolve_agent_model("claude", "parley_prepush_plan", "sonnet"),
            "haiku"
        );
        assert_eq!(cfg.parley.prepush_fix_max_chunks, 8);
        assert_eq!(cfg.parley.timeouts.prepush_plan_wall_secs, Some(120));
    }

    #[test]
    fn resolve_agent_model_tier_raw_and_defaults() {
        let cfg: Config = toml::from_str(DEFAULT_TOML).expect("parse default");
        // Unset role → session model.
        assert_eq!(
            cfg.resolve_agent_model("claude", "parley_member", "sonnet"),
            "sonnet"
        );
        // Explicit tier in [agent_models] → client model.
        assert_eq!(
            cfg.resolve_agent_model("claude", "parley_prepush_plan", "sonnet"),
            "haiku"
        );
        // Cursor xs mapping.
        assert_eq!(
            cfg.resolve_agent_model("cursor", "parley_prepush_plan", "big"),
            "composer-2-fast"
        );

        let mut cfg2 = cfg.clone();
        cfg2.agent_models.insert("parley".into(), "l".into());
        let expected_l = cfg2
            .models
            .get("claude")
            .and_then(|m| m.l.clone())
            .expect("claude l");
        for role in [
            "parley_member",
            "parley_lead",
            "parley_verifier",
            "parley_evangelist",
            "parley_repair",
            "parley_push_fix",
            "parley_prepush_plan",
        ] {
            assert_eq!(
                cfg2.resolve_agent_model("claude", role, "sonnet"),
                expected_l,
                "{role}"
            );
        }
        // Exact role still beats family catch-all.
        cfg2.agent_models
            .insert("parley_prepush_plan".into(), "xs".into());
        assert_eq!(
            cfg2.resolve_agent_model("claude", "parley_prepush_plan", "sonnet"),
            "haiku"
        );
        assert_eq!(
            cfg2.resolve_agent_model("claude", "parley_member", "sonnet"),
            expected_l
        );
        // Explicit raw model id.
        let mut cfg2 = cfg.clone();
        cfg2.agent_models
            .insert("parley_push_fix".into(), "my-custom-model".into());
        assert_eq!(
            cfg2.resolve_agent_model("claude", "parley_push_fix", "sonnet"),
            "my-custom-model"
        );
        // Plan role with no entry still defaults to xs.
        cfg2.agent_models.remove("parley_prepush_plan");
        assert_eq!(
            cfg2.resolve_agent_model("claude", "parley_prepush_plan", "sonnet"),
            "haiku"
        );
        // LOC estimate role with no entry defaults to m.
        assert_eq!(
            cfg.resolve_agent_model("claude", "forge_loc_estimate", "haiku"),
            "sonnet"
        );
        assert_eq!(
            cfg.resolve_agent_model("cursor", "forge_loc_estimate", "big"),
            cfg.models
                .get("cursor")
                .and_then(|m| m.m.clone())
                .expect("cursor m")
        );
    }

    #[test]
    fn prompts_prefix_for() {
        let empty = PromptsConfig::default();
        assert_eq!(empty.prefix_for("reviewer"), "");

        let global_only = PromptsConfig {
            global: "  G  ".into(),
            agents: BTreeMap::new(),
        };
        assert_eq!(global_only.prefix_for("reviewer"), "G");

        let mut agents = BTreeMap::new();
        agents.insert("reviewer".to_string(), "R".to_string());
        let agent_only = PromptsConfig {
            global: String::new(),
            agents: agents.clone(),
        };
        assert_eq!(agent_only.prefix_for("reviewer"), "R");
        assert_eq!(agent_only.prefix_for("security"), "");

        let both = PromptsConfig {
            global: "G".into(),
            agents,
        };
        // global first, then agent, blank-line joined.
        assert_eq!(both.prefix_for("reviewer"), "G\n\nR");
        assert_eq!(both.prefix_for("security"), "G");
    }

    #[test]
    fn parley_timeout_defaults_and_override() {
        // Unset in `[parley]` → resolved from `[timeouts]`.
        let d = ParleyConfig::default();
        assert_eq!(d.prepush_fix_wall_secs, None);
        assert_eq!(d.agent_wall_secs, None);

        // Shipped default.toml round-trips the documented values.
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, DEFAULT_TOML).unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.timeouts.parley_prepush_fix_wall_secs, Some(1200));
        assert_eq!(cfg.timeouts.parley_agent_wall_secs, Some(600));
        assert!(!cfg.git.artifact_globs.is_empty());

        // A legacy `[parley]` override is honored.
        let overridden: ParleyConfig = toml::from_str("prepush_fix_wall_secs = 300").unwrap();
        assert_eq!(overridden.prepush_fix_wall_secs, Some(300));
    }

    #[test]
    fn timeouts_section_overrides_legacy_parley_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Swap default.toml's `[timeouts]` block for custom overrides and insert a legacy
        // `prepush_fix_wall_secs` into `[parley]`.  Because `[git]` now precedes
        // `[timeouts]`, use `[probe.` as the tail anchor so `[git]` isn't duplicated.
        let head = DEFAULT_TOML.split("[timeouts]").next().unwrap();
        let tail = &DEFAULT_TOML[DEFAULT_TOML.find("[probe.").unwrap()..];
        fs::write(
            &path,
            format!(
                "{head}prepush_fix_wall_secs = 1200\n\
                 [timeouts]\nagent_wall_secs = 900\n\
                 forge_implement_wall_secs = 5400\nparley_agent_wall_secs = 120\n\n{tail}"
            ),
        )
        .unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.timeouts.agent_wall_secs, Some(900));
        assert_eq!(cfg.timeouts.parley_agent_wall_secs, Some(120));

        let t = crate::timeouts::Timeouts::resolve(&cfg.timeouts);
        assert_eq!(t.forge_implement, 5400, "explicit stage override wins");
        assert_eq!(t.forge_fix, 1800, "unset stage derives from the new base");
        assert_eq!(t.parley_agent, 120, "[timeouts] beats [parley]");
        assert_eq!(
            t.parley_prepush_fix, 1200,
            "legacy [parley] key still seeds"
        );
    }

    #[test]
    fn lifts_legacy_pr_summary_onto_probe() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Old shape: [review] pr_summary = false — must land on probe.pr_summary.
        fs::write(
            &path,
            format!(
                "{DEFAULT_TOML}\n[review]\npr_summary = false\n"
            ),
        )
        .unwrap();
        let cfg = load_config(&path).unwrap();
        assert!(!cfg.probe.pr_summary);
    }

    #[test]
    fn load_config_populates_prompt_overrides() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let toml = format!(
            "{DEFAULT_TOML}\n[prompts]\nglobal = \"GLOB\"\n[prompts.agents]\nreviewer = \"REV\"\n"
        );
        fs::write(&path, toml).unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.prompts.global, "GLOB");
        // Global store updated as a side effect of load_config.
        assert_eq!(resolve_prompt_prefix("reviewer"), "GLOB\n\nREV");
        assert_eq!(resolve_prompt_prefix("security"), "GLOB");
    }

    #[test]
    fn merge_toml_overrides_per_item() {
        let mut base: toml::Value = toml::from_str(
            "default_client = \"claude\"\nheadless = true\n[models.claude]\nm = \"g-m\"\nl = \"g-l\"\n[git]\nbase_candidates = [\"main\", \"master\"]\n",
        )
        .unwrap();
        let over: toml::Value = toml::from_str(
            "default_client = \"codex\"\n[models.claude]\nm = \"local-m\"\n[git]\nbase_candidates = [\"develop\"]\n",
        )
        .unwrap();
        merge_toml(&mut base, over);

        // scalar overridden
        assert_eq!(base["default_client"].as_str(), Some("codex"));
        // untouched scalar preserved
        assert_eq!(base["headless"].as_bool(), Some(true));
        // nested table: overridden key wins, sibling preserved
        assert_eq!(base["models"]["claude"]["m"].as_str(), Some("local-m"));
        assert_eq!(base["models"]["claude"]["l"].as_str(), Some("g-l"));
        // array replaced, not appended
        let bases = base["git"]["base_candidates"].as_array().unwrap();
        assert_eq!(bases.len(), 1);
        assert_eq!(bases[0].as_str(), Some("develop"));
    }

    #[test]
    fn merged_value_deserializes_with_defaults() {
        // Local override applied to the default config, then deserialized.
        let mut base: toml::Value = toml::from_str(DEFAULT_TOML).unwrap();
        let over: toml::Value = toml::from_str("default_client = \"codex\"\n").unwrap();
        merge_toml(&mut base, over);
        let cfg: Config = base.try_into().unwrap();
        assert_eq!(cfg.default_client, "codex");
        // untouched fields still come from global/defaults
        assert_eq!(cfg.probe.agents.reviewers_by_tier.get(Tier::Xs), 0);
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
