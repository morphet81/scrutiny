//! Scrutiny core: config, git, eval, map, pack, scan, plan, forge, findings, review session.

pub mod config;
pub mod eval;
pub mod findings;
pub mod forge;
pub mod git;
pub mod map;
pub mod pack;
pub mod paths;
pub mod plan;
pub mod review_session;
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
pub use plan::{
    load_plan_answers, run_plan_confirm, run_plan_write, ConfirmedPlan, PlanAnswers,
    PlanConfirmInput, PlanWriteInput,
};
pub use review_session::{
    partition_pack_paths, run_review_session_write, ReviewSession, ReviewSessionWriteInput,
};
pub use scan::{normalize_severity, run_scan, ScanReport};
