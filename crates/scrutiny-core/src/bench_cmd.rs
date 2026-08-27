//! `scrutiny bench` — token-usage compare across cli / skill / skill+caveman.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::agent_runner::{
    dedupe_usage_records, start_usage_capture, sum_usage_records, take_usage_capture, TokenUsage,
    UsageRecord,
};
use crate::forge_cmd::{run_forge, ForgeCmdInput};
use crate::paths::write_json_pretty;
use crate::review_cmd::{run_review, ReviewCmdInput};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchWorkload {
    Probe,
    Forge,
    Both,
}

impl BenchWorkload {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "probe" => Ok(Self::Probe),
            "forge" => Ok(Self::Forge),
            "both" => Ok(Self::Both),
            other => bail!("unknown workload {other:?} (probe|forge|both)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BenchArm {
    Cli,
    Skill,
    SkillCaveman,
}

impl BenchArm {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cli" => Ok(Self::Cli),
            "skill" => Ok(Self::Skill),
            "skill-caveman" | "skill_caveman" | "skill+caveman" => Ok(Self::SkillCaveman),
            other => bail!("unknown arm {other:?} (cli|skill|skill-caveman)"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Skill => "skill",
            Self::SkillCaveman => "skill-caveman",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchCmdInput {
    pub cwd: PathBuf,
    pub workload: BenchWorkload,
    pub arms: Vec<BenchArm>,
    /// PlanAnswers JSON for probe.
    pub from_json: Option<String>,
    /// Forge knobs JSON.
    pub forge_from_json: Option<String>,
    pub forge_input: Option<String>,
    pub model: Option<String>,
    pub out: PathBuf,
    pub pr: Option<String>,
    /// Use built-in smoke fixtures under a disposable worktree (recommended).
    pub use_fixtures: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmUsageReport {
    pub arm: String,
    pub workload: String,
    pub usage: TokenUsage,
    pub wall_ms: u64,
    pub calls: Vec<UsageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchReport {
    pub version: u32,
    pub model: String,
    pub arms: Vec<ArmUsageReport>,
}

pub fn run_bench(input: BenchCmdInput) -> Result<PathBuf> {
    fs::create_dir_all(&input.out)
        .with_context(|| format!("create bench out {}", input.out.display()))?;

    let model = input.model.clone().unwrap_or_else(|| "sonnet".to_string());

    let workloads: Vec<BenchWorkload> = match input.workload {
        BenchWorkload::Both => vec![BenchWorkload::Probe, BenchWorkload::Forge],
        w => vec![w],
    };

    let mut arms_out: Vec<ArmUsageReport> = Vec::new();

    for wl in workloads {
        for arm in &input.arms {
            let label = format!("{} / {}", wl_name(wl), arm.as_str());
            eprintln!("scrutiny bench: run {label}…");
            let arm_dir = input
                .out
                .join(format!("arm-{}-{}", wl_name(wl), arm.as_str()));
            fs::create_dir_all(&arm_dir)?;

            let started = Instant::now();
            let result = run_one_arm(&input, wl, *arm, &arm_dir, &model);
            let wall_ms = started.elapsed().as_millis() as u64;

            let report = match result {
                Ok((calls, err)) => {
                    let usage = sum_usage_records(&calls);
                    ArmUsageReport {
                        arm: arm.as_str().into(),
                        workload: wl_name(wl).into(),
                        usage,
                        wall_ms,
                        calls: dedupe_usage_records(&calls),
                        error: err,
                    }
                }
                Err(e) => ArmUsageReport {
                    arm: arm.as_str().into(),
                    workload: wl_name(wl).into(),
                    usage: TokenUsage::default(),
                    wall_ms,
                    calls: take_usage_capture(),
                    error: Some(format!("{e:#}")),
                },
            };
            write_json_pretty(&arm_dir.join("usage.json"), &report)?;
            arms_out.push(report);
        }
    }

    let bench = BenchReport {
        version: 1,
        model,
        arms: arms_out,
    };
    let report_path = input.out.join("bench-report.json");
    write_json_pretty(&report_path, &bench)?;
    print_table(&bench);
    Ok(report_path)
}

fn wl_name(w: BenchWorkload) -> &'static str {
    match w {
        BenchWorkload::Probe => "probe",
        BenchWorkload::Forge => "forge",
        BenchWorkload::Both => "both",
    }
}

fn run_one_arm(
    input: &BenchCmdInput,
    workload: BenchWorkload,
    arm: BenchArm,
    arm_dir: &Path,
    model: &str,
) -> Result<(Vec<UsageRecord>, Option<String>)> {
    // Fresh capture per arm.
    let _ = take_usage_capture();
    start_usage_capture();

    let work = if input.use_fixtures {
        prepare_fixture_workdir(workload, arm_dir)?
    } else {
        input.cwd.clone()
    };

    // Arm-specific env.
    clear_arm_env();
    match arm {
        BenchArm::Cli => {
            // Product defaults: caveman on, no skill preamble.
        }
        BenchArm::Skill => {
            std::env::set_var("SCRUTINY_NO_CAVEMAN", "1");
            if let Some(skill) = skill_md_path(workload)? {
                std::env::set_var("SCRUTINY_BENCH_SKILL_PREAMBLE", &skill);
            }
        }
        BenchArm::SkillCaveman => {
            if let Some(skill) = skill_md_path(workload)? {
                std::env::set_var("SCRUTINY_BENCH_SKILL_PREAMBLE", &skill);
            }
        }
    }

    let run_result = match workload {
        BenchWorkload::Probe => run_probe_arm(input, arm, &work, model),
        BenchWorkload::Forge => run_forge_arm(input, arm, &work, model),
        BenchWorkload::Both => unreachable!(),
    };

    clear_arm_env();
    let calls = take_usage_capture();
    // Keep partial usage even when the arm errors (verify/commit/etc.).
    if let Err(e) = run_result {
        return Ok((calls, Some(format!("{e:#}"))));
    }
    Ok((calls, None))
}

fn clear_arm_env() {
    std::env::remove_var("SCRUTINY_NO_CAVEMAN");
    std::env::remove_var("SCRUTINY_BENCH_SKILL_PREAMBLE");
}

fn run_probe_arm(input: &BenchCmdInput, _arm: BenchArm, cwd: &Path, model: &str) -> Result<()> {
    // All arms use the same orchestrated probe path so pack/plan match.
    // Style + skill preamble differ via env (inject_overrides).
    let from_json = input.from_json.clone().unwrap_or_else(|| {
        serde_json::json!({
            "client": "claude",
            "model": model,
            "security": false,
            "performance": false,
            "error_handling": false,
            "reviewers": 1,
            "evangelists": 0,
            "spawn_mode": "isolated"
        })
        .to_string()
    });

    if input.use_fixtures {
        std::env::set_var("BASE_BRANCH", "main");
    }

    let result = run_review(ReviewCmdInput {
        cwd: cwd.to_path_buf(),
        pr: input.pr.clone(),
        client: Some("claude".into()),
        spawn_mode: Some("isolated".into()),
        from_json: Some(from_json),
        skip_agents: false,
        event: None,
        non_interactive: true,
        from_report: None,
        scan_path: None,
        skip_triage: false,
    });

    if input.use_fixtures {
        std::env::remove_var("BASE_BRANCH");
    }
    result?;
    Ok(())
}

fn run_forge_arm(input: &BenchCmdInput, _arm: BenchArm, cwd: &Path, model: &str) -> Result<()> {
    let from_json = input.forge_from_json.clone().unwrap_or_else(|| {
        serde_json::json!({
            "client": "claude",
            "model": model,
            "spawn_mode": "single",
            "use_playwright": false,
            "tdd": false,
            "coverage_pct": 0,
            "e2e": false,
            "agents": 1,
            "testers": 0,
            "reviewers": 0,
            "evangelists": 0
        })
        .to_string()
    });

    let forge_input = input
        .forge_input
        .clone()
        .unwrap_or_else(|| {
            "Add a public function `pub fn add(a: i32, b: i32) -> i32` in src/lib.rs that returns a+b. Write a unit test. Do not create a git branch.".into()
        });

    let _ = run_forge(ForgeCmdInput {
        cwd: cwd.to_path_buf(),
        input: Some(forge_input),
        inline: true,
        source: None,
        client: Some("claude".into()),
        title: Some("bench-inline".into()),
        from_json: Some(from_json),
        non_interactive: true,
    })?;
    Ok(())
}

fn skill_md_path(workload: BenchWorkload) -> Result<Option<PathBuf>> {
    let name = match workload {
        BenchWorkload::Probe => "scrutiny",
        BenchWorkload::Forge => "forge",
        BenchWorkload::Both => return Ok(None),
    };
    if let Ok(root) = std::env::var("SCRUTINY_SKILLS_ROOT") {
        let p = PathBuf::from(root).join(name).join("SKILL.md");
        if p.is_file() {
            return Ok(Some(p));
        }
    }
    // Walk up from CARGO_MANIFEST_DIR / current_exe for skills/<name>/SKILL.md
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.to_path_buf());
            if let Some(p) = dir.parent() {
                candidates.push(p.to_path_buf());
                if let Some(pp) = p.parent() {
                    candidates.push(pp.to_path_buf());
                }
            }
        }
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
    candidates.push(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    for base in candidates {
        let p = base.join("skills").join(name).join("SKILL.md");
        if p.is_file() {
            return Ok(Some(p.canonicalize().unwrap_or(p)));
        }
    }
    eprintln!("scrutiny bench: warn: skills/{name}/SKILL.md not found — skill preamble skipped");
    Ok(None)
}

/// Disposable mini repo: one commit on main, dirty (or second commit) change for probe.
fn prepare_fixture_workdir(workload: BenchWorkload, arm_dir: &Path) -> Result<PathBuf> {
    let work = arm_dir.join("workdir");
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    fs::create_dir_all(work.join("src"))?;

    fs::write(
        work.join("Cargo.toml"),
        "[package]\nname = \"bench_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    fs::write(
        work.join("src/lib.rs"),
        "//! Bench fixture.\n\npub fn greet(name: &str) -> String {\n    format!(\"hi {name}\")\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn greet_ok() {\n        assert_eq!(greet(\"a\"), \"hi a\");\n    }\n}\n",
    )?;
    fs::write(work.join(".gitignore"), "/target\n.scrutiny/\n")?;

    git(&work, &["init", "-b", "main"])?;
    git(&work, &["config", "user.email", "bench@scrutiny.local"])?;
    git(&work, &["config", "user.name", "scrutiny-bench"])?;
    git(&work, &["add", "-A"])?;
    git(&work, &["commit", "-m", "chore: fixture base"])?;

    match workload {
        BenchWorkload::Probe => {
            // Branch off main so local probe has a clear base.
            git(&work, &["checkout", "-b", "feat/bench-shout"])?;
            fs::write(
                work.join("src/lib.rs"),
                "//! Bench fixture.\n\npub fn greet(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n\npub fn shout(name: &str) -> String {\n    greet(name).to_uppercase()\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn greet_ok() {\n        assert_eq!(greet(\"a\"), \"hello a\");\n    }\n    #[test]\n    fn shout_ok() {\n        assert_eq!(shout(\"a\"), \"HELLO A\");\n    }\n}\n",
            )?;
            git(&work, &["add", "-A"])?;
            git(&work, &["commit", "-m", "feat: shout helper"])?;
        }
        BenchWorkload::Forge => {
            // Clean tree; forge --inline will edit.
        }
        BenchWorkload::Both => {}
    }

    Ok(work)
}

fn git(cwd: &Path, args: &[&str]) -> Result<()> {
    let st = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !st.success() {
        bail!("git {} failed in {}", args.join(" "), cwd.display());
    }
    Ok(())
}

fn print_table(report: &BenchReport) {
    eprintln!();
    eprintln!(
        "{:<8} {:<14} {:>10} {:>12} {:>10} {:>10} {:>8}",
        "workload", "arm", "in", "cache_read", "out", "total", "vs_cli"
    );
    // Baseline total per workload = cli arm.
    for wl in ["probe", "forge"] {
        let cli_total = report
            .arms
            .iter()
            .find(|a| a.workload == wl && a.arm == "cli" && a.error.is_none())
            .map(|a| a.usage.total());

        for a in report.arms.iter().filter(|a| a.workload == wl) {
            let vs = if a.arm == "cli" {
                "—".to_string()
            } else if let (Some(base), true) = (cli_total, a.error.is_none()) {
                if base == 0 {
                    "n/a".into()
                } else {
                    let delta = (a.usage.total() as f64 - base as f64) * 100.0 / base as f64;
                    format!("{delta:+.1}%")
                }
            } else {
                "err".into()
            };
            let err_mark = if a.error.is_some() { "!" } else { "" };
            eprintln!(
                "{:<8} {:<14} {:>10} {:>12} {:>10} {:>10} {:>8}{}",
                a.workload,
                a.arm,
                a.usage.input_tokens,
                a.usage.cache_read_input_tokens,
                a.usage.output_tokens,
                a.usage.total(),
                vs,
                err_mark
            );
            if let Some(e) = &a.error {
                eprintln!("         error: {e}");
            }
        }
    }
    eprintln!();
    eprintln!("scrutiny bench: report → (printed path on stdout)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_arms() {
        assert_eq!(BenchArm::parse("cli").unwrap(), BenchArm::Cli);
        assert_eq!(
            BenchArm::parse("skill-caveman").unwrap(),
            BenchArm::SkillCaveman
        );
    }

    #[test]
    fn fixture_workdir_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let arm = tmp.path().join("arm");
        fs::create_dir_all(&arm).unwrap();
        let work = prepare_fixture_workdir(BenchWorkload::Probe, &arm).unwrap();
        assert!(work.join("src/lib.rs").is_file());
        let log = Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&work)
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&log.stdout);
        assert!(s.contains("feat: shout"));
    }
}
