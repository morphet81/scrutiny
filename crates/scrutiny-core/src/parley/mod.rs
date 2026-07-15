//! Address unresolved PR review comments (`scrutiny parley`).

pub mod fetch;
pub mod fixes;
pub mod plan;
pub mod reply;

pub use fetch::{run_parley_fetch, ParleyComment, ParleyCommentsFile, ParleyFetchInput};
pub use fixes::{
    init_fixes_file, load_fixes, merge_fix_entries, validate_fixes_complete, FixEntry,
    ParleyFixesFile,
};
pub use plan::{
    partition_comments, prompt_parley_answers, run_parley_plan_write, ParleyAnswers,
    ParleyPlan, ParleyPlanWriteInput,
};
pub use reply::{run_parley_reply, ParleyReplyInput, ParleyReplyResult};
