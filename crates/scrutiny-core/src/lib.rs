//! Scrutiny core: config, git, eval, map, pack, scan, plan, forge, parley, findings, review.

pub mod agent_runner;
pub mod config;
pub mod diff_loc;
pub mod eval;
pub mod findings;
pub mod forge;
pub mod forge_cmd;
pub mod gh;
pub mod git;
pub mod map;
pub mod mdterm;
pub mod pack;
pub mod parley;
pub mod parley_cmd;
pub mod paths;
pub mod plan;
pub mod pr;
pub mod pr_cmd;
pub mod review_cmd;
pub mod review_session;
pub mod runtime;
pub mod scan;
pub mod score;
pub mod signals;
pub mod skills_install;
pub mod taxonomy;
pub mod terminal;
pub mod treesitter;

pub use config::{ensure_config, find_shipped_default, load_config, Config};
pub use eval::{run_eval, EvalInput, EvalReport};
pub use findings::{
    attach_pr_to_findings, merge_ai_findings, prompt_pr_if_missing, run_findings_init,
    run_findings_init_empty, run_findings_resolve, run_findings_triage, run_findings_validate,
    run_post_comments, FindingsInitInput, FindingsReport, PostCommentsInput, PostResult,
    TriageAskCtx,
};
pub use forge::{
    run_forge_brief, run_forge_context, run_forge_fetch, run_forge_plan_write, ForgeFetchInput,
    ForgePlanWriteInput, ForgeSessionPlan, TicketReport,
};
pub use forge_cmd::{run_forge, ForgeCmdInput};
pub use map::{run_map, MapReport};
pub use parley::{
    run_parley_fetch, run_parley_plan_write, run_parley_reply, ParleyAnswers, ParleyFetchInput,
    ParleyPlanWriteInput, ParleyReplyInput,
};
pub use parley_cmd::{run_parley, ParleyCmdInput};
pub use pack::{run_pack, PackReport};
pub use paths::{
    prepare_artifacts, temp_artifact_path, warn_if_scrutiny_unignored, init_artifact_ctx,
};
pub use plan::{
    load_plan_answers, run_plan_confirm, run_plan_write, ConfirmedPlan, PlanAnswers,
    PlanConfirmInput, PlanWriteInput,
};
pub use pr_cmd::{run_pr, PrCmdInput};
pub use agent_runner::{
    build_isolated_prompt, build_team_lead_prompt, AgentPromptInput, run_agent_prompt,
};
pub use review_cmd::{run_review, run_review_from_report, ReportResumeInput, ReviewCmdInput};
pub use review_session::{
    partition_pack_paths, run_review_session_write, ReviewSession, ReviewSessionWriteInput,
};
pub use runtime::{detect_clients, resolve_client, normalize_spawn_mode, DetectedClient};
pub use scan::{normalize_severity, run_scan, ScanReport};
pub use skills_install::{run_skills_install, SkillsInstallInput};
