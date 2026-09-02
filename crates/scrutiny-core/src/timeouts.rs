//! Resolved agent wall-clock timeouts.
//!
//! Every stage wall used to be a `const` derived from `AGENT_WALL_SECS`, so the
//! only way to raise one was a rebuild. `load_config` now installs a resolved
//! table here and the call sites read it, which keeps the ~15 spawn sites free
//! of a threaded-through `Config`.

use std::sync::OnceLock;
use std::time::Duration;

use crate::config::TimeoutsConfig;

pub const DEFAULT_AGENT_WALL_SECS: u64 = 10 * 60;
pub const DEFAULT_PROGRESS_SECS: u64 = 15;
pub const DEFAULT_HEADLESS_FIRST_OUTPUT_SECS: u64 = 90;

/// Multipliers applied to `agent_wall_secs` when a stage has no explicit
/// override. These are the historical hardcoded ratios.
const NONHEADLESS_X: u64 = 3;
const IMPLEMENT_X: u64 = 2;
const FIX_X: u64 = 2;
const BULK_ITEM_X: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub agent: u64,
    pub progress: u64,
    pub nonheadless: u64,
    pub probe_isolated: u64,
    pub probe_team: u64,
    pub probe_consolidate: u64,
    pub probe_ask: u64,
    pub probe_summary: u64,
    pub forge_test_plan: u64,
    pub forge_loc_estimate: u64,
    pub forge_pr_description: u64,
    pub forge_implement: u64,
    pub forge_fix: u64,
    pub forge_bulk_item: u64,
    pub parley_agent: u64,
    pub parley_prepush_fix: u64,
    pub parley_prepush_plan: u64,
    /// 0 = disabled (wait full wall even with empty stdout).
    pub headless_first_output: u64,
}

impl Timeouts {
    /// Resolve every stage: explicit override wins, else `agent_wall_secs`
    /// times the stage's historical multiplier.
    pub fn resolve(cfg: &TimeoutsConfig) -> Self {
        let base = nonzero(cfg.agent_wall_secs, DEFAULT_AGENT_WALL_SECS);
        let mul = |x: u64| base.saturating_mul(x);
        Self {
            agent: base,
            progress: nonzero(cfg.progress_secs, DEFAULT_PROGRESS_SECS),
            nonheadless: nonzero(cfg.nonheadless_wall_secs, mul(NONHEADLESS_X)),
            probe_isolated: nonzero(cfg.probe_isolated_wall_secs, base),
            probe_team: nonzero(cfg.probe_team_wall_secs, base),
            probe_consolidate: nonzero(cfg.probe_consolidate_wall_secs, base),
            probe_ask: nonzero(cfg.probe_ask_wall_secs, base),
            probe_summary: nonzero(cfg.probe_summary_wall_secs, base),
            forge_test_plan: nonzero(cfg.forge_test_plan_wall_secs, base),
            forge_loc_estimate: nonzero(cfg.forge_loc_estimate_wall_secs, base),
            forge_pr_description: nonzero(cfg.forge_pr_description_wall_secs, base),
            forge_implement: nonzero(cfg.forge_implement_wall_secs, mul(IMPLEMENT_X)),
            forge_fix: nonzero(cfg.forge_fix_wall_secs, mul(FIX_X)),
            forge_bulk_item: nonzero(cfg.forge_bulk_item_wall_secs, mul(BULK_ITEM_X)),
            parley_agent: nonzero(cfg.parley_agent_wall_secs, base),
            parley_prepush_fix: nonzero(cfg.parley_prepush_fix_wall_secs, mul(IMPLEMENT_X)),
            // Plan agent is short/read-only — fixed 120s default, not base-derived.
            parley_prepush_plan: nonzero(cfg.parley_prepush_plan_wall_secs, 120),
            // Explicit 0 disables early kill; unset → default 90.
            headless_first_output: match cfg.headless_first_output_secs {
                Some(0) => 0,
                Some(n) => n,
                None => DEFAULT_HEADLESS_FIRST_OUTPUT_SECS,
            },
        }
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self::resolve(&TimeoutsConfig::default())
    }
}

fn nonzero(v: Option<u64>, fallback: u64) -> u64 {
    match v {
        Some(n) if n > 0 => n,
        _ => fallback,
    }
}

static RESOLVED: OnceLock<Timeouts> = OnceLock::new();

/// Publish the resolved table. First call wins — every `load_config` reads the
/// same file, so a later call would install identical values anyway.
pub fn install(t: Timeouts) {
    let _ = RESOLVED.set(t);
}

pub fn get() -> Timeouts {
    *RESOLVED.get_or_init(Timeouts::default)
}

macro_rules! wall_accessors {
    ($($name:ident => $field:ident),* $(,)?) => {
        $(
            pub fn $name() -> Duration {
                Duration::from_secs(get().$field)
            }
        )*
    };
}

wall_accessors! {
    agent => agent,
    progress => progress,
    nonheadless => nonheadless,
    probe_isolated => probe_isolated,
    probe_team => probe_team,
    probe_consolidate => probe_consolidate,
    probe_ask => probe_ask,
    probe_summary => probe_summary,
    forge_test_plan => forge_test_plan,
    forge_loc_estimate => forge_loc_estimate,
    forge_pr_description => forge_pr_description,
    forge_implement => forge_implement,
    forge_fix => forge_fix,
    forge_bulk_item => forge_bulk_item,
    parley_agent => parley_agent,
    parley_prepush_fix => parley_prepush_fix,
    parley_prepush_plan => parley_prepush_plan,
    headless_first_output => headless_first_output,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_historical_constants() {
        let t = Timeouts::default();
        assert_eq!(t.agent, 600);
        assert_eq!(t.progress, 15);
        assert_eq!(t.nonheadless, 1800);
        assert_eq!(t.probe_isolated, 600);
        assert_eq!(t.probe_team, 600);
        assert_eq!(t.probe_consolidate, 600);
        assert_eq!(t.probe_ask, 600);
        assert_eq!(t.probe_summary, 600);
        assert_eq!(t.forge_test_plan, 600);
        assert_eq!(t.forge_loc_estimate, 600);
        assert_eq!(t.forge_pr_description, 600);
        assert_eq!(t.forge_implement, 1200);
        assert_eq!(t.forge_fix, 1200);
        assert_eq!(t.forge_bulk_item, 4800);
        assert_eq!(t.parley_agent, 600);
        assert_eq!(t.parley_prepush_fix, 1200);
        assert_eq!(t.parley_prepush_plan, 120);
        assert_eq!(t.headless_first_output, DEFAULT_HEADLESS_FIRST_OUTPUT_SECS);
    }

    #[test]
    fn headless_first_output_zero_disables() {
        let t = Timeouts::resolve(&TimeoutsConfig {
            headless_first_output_secs: Some(0),
            ..Default::default()
        });
        assert_eq!(t.headless_first_output, 0);
    }

    #[test]
    fn base_scales_every_derived_stage() {
        let t = Timeouts::resolve(&TimeoutsConfig {
            agent_wall_secs: Some(1800),
            ..Default::default()
        });
        assert_eq!(t.agent, 1800);
        assert_eq!(t.forge_implement, 3600);
        assert_eq!(t.forge_fix, 3600);
        assert_eq!(t.nonheadless, 5400);
        assert_eq!(t.forge_bulk_item, 14400);
        assert_eq!(t.probe_isolated, 1800);
    }

    #[test]
    fn per_stage_override_beats_base() {
        let t = Timeouts::resolve(&TimeoutsConfig {
            agent_wall_secs: Some(600),
            forge_implement_wall_secs: Some(5400),
            ..Default::default()
        });
        assert_eq!(t.forge_implement, 5400);
        assert_eq!(t.forge_fix, 1200, "other stages stay on the base");
        assert_eq!(t.agent, 600);
    }

    #[test]
    fn zero_means_unset() {
        let t = Timeouts::resolve(&TimeoutsConfig {
            agent_wall_secs: Some(0),
            progress_secs: Some(0),
            forge_implement_wall_secs: Some(0),
            probe_ask_wall_secs: Some(0),
            ..Default::default()
        });
        assert_eq!(t.agent, DEFAULT_AGENT_WALL_SECS);
        assert_eq!(t.progress, DEFAULT_PROGRESS_SECS);
        assert_eq!(t.forge_implement, 1200);
        assert_eq!(t.probe_ask, 600);
    }

    #[test]
    fn accessors_return_resolved_durations() {
        install(Timeouts::default());
        assert_eq!(agent(), Duration::from_secs(get().agent));
        assert_eq!(
            forge_implement(),
            Duration::from_secs(get().forge_implement)
        );
    }
}
