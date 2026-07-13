//! Scrutiny core: config, git base detection, complexity eval, change map, pack, scan, plan.

pub mod config;
pub mod eval;
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
pub use map::{run_map, MapReport};
pub use pack::{run_pack, PackReport};
pub use paths::temp_artifact_path;
pub use plan::{run_plan_write, ConfirmedPlan, PlanWriteInput};
pub use scan::{run_scan, ScanReport};
