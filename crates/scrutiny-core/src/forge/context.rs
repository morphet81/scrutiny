use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::forge::fetch::TicketReport;
use crate::paths::{temp_artifact_path, write_json_pretty};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeContextReport {
    pub version: u32,
    pub ticket_path: String,
    pub cwd: String,
    pub keywords: Vec<String>,
    pub related_paths: Vec<String>,
    pub test_harness: TestHarnessHints,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TestHarnessHints {
    pub unit_framework: Option<String>,
    pub e2e_framework: Option<String>,
    pub test_dirs: Vec<String>,
    pub config_files: Vec<String>,
}

pub fn run_forge_context(ticket_path: &Path, cwd: &Path) -> Result<(ForgeContextReport, PathBuf)> {
    let ticket: TicketReport = serde_json::from_str(
        &fs::read_to_string(ticket_path)
            .with_context(|| format!("read ticket {}", ticket_path.display()))?,
    )
    .context("parse ticket json")?;

    let keywords = keywords_from_ticket(&ticket);
    let related_paths = find_related_paths(cwd, &keywords);
    let test_harness = sniff_test_harness(cwd);
    let mut notes = Vec::new();
    if related_paths.is_empty() {
        notes.push("no keyword path hits; agents should use brief + targeted search".into());
    }

    let report = ForgeContextReport {
        version: 1,
        ticket_path: ticket_path.display().to_string(),
        cwd: cwd.display().to_string(),
        keywords,
        related_paths,
        test_harness,
        notes,
    };
    let path = temp_artifact_path("forge", &ticket.id, "context");
    write_json_pretty(&path, &report)?;
    Ok((report, path))
}

fn keywords_from_ticket(ticket: &TicketReport) -> Vec<String> {
    let mut words = Vec::new();
    let mut push_token = |t: &str| {
        let t = t.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        if t.len() < 3 {
            return;
        }
        let lower = t.to_ascii_lowercase();
        const STOP: &[&str] = &[
            "the", "and", "for", "with", "that", "this", "from", "into", "should", "when",
            "have", "will", "must", "need", "also", "user", "users", "page", "able",
        ];
        if STOP.contains(&lower.as_str()) {
            return;
        }
        if !words.iter().any(|w| w == &lower) {
            words.push(lower);
        }
    };

    for part in ticket.title.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-') {
        push_token(part);
    }
    for line in ticket.description.lines().take(20) {
        for part in line.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-') {
            push_token(part);
        }
    }
    words.into_iter().take(12).collect()
}

fn find_related_paths(cwd: &Path, keywords: &[String]) -> Vec<String> {
    if keywords.is_empty() {
        return Vec::new();
    }
    // Prefer ripgrep if available
    if Command::new("rg").arg("--version").output().is_ok() {
        let pattern = keywords
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join("|");
        let output = Command::new("rg")
            .args([
                "-l",
                "-i",
                "--glob",
                "!**/node_modules/**",
                "--glob",
                "!**/target/**",
                "--glob",
                "!**/.git/**",
                "--glob",
                "!**/dist/**",
                "-m",
                "1",
                &pattern,
            ])
            .current_dir(cwd)
            .output();
        if let Ok(out) = output {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                return text
                    .lines()
                    .take(40)
                    .map(|s| s.to_string())
                    .collect();
            }
        }
    }
    // Fallback: walk shallow for filename hits
    let mut hits = Vec::new();
    let _ = walk_names(cwd, cwd, keywords, &mut hits, 0);
    hits.into_iter().take(40).collect()
}

fn walk_names(
    root: &Path,
    dir: &Path,
    keywords: &[String],
    hits: &mut Vec<String>,
    depth: usize,
) -> Result<()> {
    if depth > 4 || hits.len() >= 40 {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for ent in entries.flatten() {
        let name = ent.file_name().to_string_lossy().to_ascii_lowercase();
        if name == "node_modules" || name == "target" || name == ".git" || name == "dist" {
            continue;
        }
        let path = ent.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        if keywords.iter().any(|k| name.contains(k)) {
            hits.push(rel.clone());
        }
        if path.is_dir() {
            walk_names(root, &path, keywords, hits, depth + 1)?;
        }
    }
    Ok(())
}

fn sniff_test_harness(cwd: &Path) -> TestHarnessHints {
    let mut hints = TestHarnessHints::default();
    let candidates = [
        ("playwright.config.ts", "playwright"),
        ("playwright.config.js", "playwright"),
        ("cypress.config.ts", "cypress"),
        ("cypress.config.js", "cypress"),
        ("jest.config.ts", "jest"),
        ("jest.config.js", "jest"),
        ("vitest.config.ts", "vitest"),
        ("vitest.config.js", "vitest"),
        ("Cargo.toml", "cargo-test"),
        ("phpunit.xml", "phpunit"),
        ("pytest.ini", "pytest"),
    ];
    for (file, framework) in candidates {
        if cwd.join(file).exists() {
            hints.config_files.push(file.into());
            if framework == "playwright" || framework == "cypress" {
                hints.e2e_framework = Some(framework.into());
            } else if hints.unit_framework.is_none() {
                hints.unit_framework = Some(framework.into());
            }
        }
    }
    for dir in ["e2e", "tests", "test", "__tests__", "spec", "cypress"] {
        if cwd.join(dir).is_dir() {
            hints.test_dirs.push(dir.into());
        }
    }
    // package.json scripts sniff
    let pkg = cwd.join("package.json");
    if let Ok(text) = fs::read_to_string(pkg) {
        if text.contains("vitest") && hints.unit_framework.is_none() {
            hints.unit_framework = Some("vitest".into());
        }
        if text.contains("jest") && hints.unit_framework.is_none() {
            hints.unit_framework = Some("jest".into());
        }
        if text.contains("playwright") && hints.e2e_framework.is_none() {
            hints.e2e_framework = Some("playwright".into());
        }
    }
    hints
}
