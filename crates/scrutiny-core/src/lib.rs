//! Scrutiny core: config, git, eval, map, pack, scan, plan, forge, findings.

pub mod config;
pub mod eval;
pub mod findings;
pub mod forge;
pub mod git;
pub mod map;
pub mod pack;
pub mod paths;
pub mod plan;
pub mod scan;
pub mod score;
pub mod taxonomy;

pub use config::{ensure_config, load_config, Config};
pub use eval::{run_eval, EvalInput, EvalReport};
pub use findings::{
    run_findings_init, run_findings_resolve, run_findings_validate, run_post_comments,
    FindingsInitInput, FindingsReport, PostCommentsInput, PostResult,
};
pub use forge::{
    run_forge_brief, run_forge_context, run_forge_fetch, run_forge_plan_write, ForgeFetchInput,
    ForgePlanWriteInput, ForgeSessionPlan, TicketReport,
};
pub use map::{run_map, MapReport};
pub use pack::{run_pack, PackReport};
pub use paths::temp_artifact_path;
pub use plan::{run_plan_write, ConfirmedPlan, PlanWriteInput};
pub use scan::{normalize_severity, run_scan, ScanReport};
