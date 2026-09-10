//! Address unresolved PR review comments (`scrutiny parley`).

pub mod fetch;
pub mod fixes;
pub mod plan;
pub mod reply;

pub use fetch::{
    pr_has_unresolved_comments, run_parley_fetch, ParleyComment, ParleyCommentsFile,
    ParleyFetchInput,
};
pub use fixes::{
    init_fixes_file, load_fixes, merge_fix_entries, validate_fixes_complete, FixEntry,
    ParleyFixesFile,
};
pub use plan::{
    partition_comments, prompt_parley_answers, run_parley_plan_write, ParleyAnswers, ParleyPlan,
    ParleyPlanWriteInput,
};
pub use reply::{
    discover_parley_fixes, resolve_parley_fixes_path, run_parley_reply, ParleyReplyInput,
    ParleyReplyResult,
};
