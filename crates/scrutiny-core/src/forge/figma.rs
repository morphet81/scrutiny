//! Export Figma designs via fcli into the forge session directory.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::forge::tools::require_fcli;
use crate::paths::write_json_pretty;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FigmaExportReport {
    pub version: u32,
    pub urls: Vec<String>,
    pub dir: String,
    pub files: Vec<FigmaFileExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FigmaFileExport {
    pub url: String,
    pub inspect_path: Option<String>,
    pub export_dir: Option<String>,
    pub error: Option<String>,
}

/// Require fcli when URLs non-empty; export inspect XML/tree + PNG into `figma_dir`.
pub fn export_figma_designs(
    cwd: &Path,
    session_root: &Path,
    urls: &[String],
) -> Result<Option<FigmaExportReport>> {
    if urls.is_empty() {
        return Ok(None);
    }
    require_fcli()?;

    let figma_dir = session_root.join("figma");
    fs::create_dir_all(&figma_dir).context("create figma dir")?;

    let mut files = Vec::new();
    for (i, url) in urls.iter().enumerate() {
        let slot = figma_dir.join(format!("design-{i}"));
        fs::create_dir_all(&slot).ok();
        let mut entry = FigmaFileExport {
            url: url.clone(),
            inspect_path: None,
            export_dir: None,
            error: None,
        };

        let inspect_out = slot.join("structure.txt");
        let inspect = Command::new("fcli")
            .args([
                "file",
                "inspect",
                "--url",
                url,
                "--depth",
                "8",
            ])
            .current_dir(cwd)
            .output();
        match inspect {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                fs::write(&inspect_out, text.as_bytes()).ok();
                // Also try JSON structure
                let json_out = slot.join("structure.json");
                let inspect_json = Command::new("fcli")
                    .args(["file", "inspect", "--url", url, "--depth", "8", "--json"])
                    .current_dir(cwd)
                    .output();
                if let Ok(jo) = inspect_json {
                    if jo.status.success() {
                        let _ = fs::write(&json_out, &jo.stdout);
                        entry.inspect_path = Some(json_out.display().to_string());
                    } else {
                        entry.inspect_path = Some(inspect_out.display().to_string());
                    }
                } else {
                    entry.inspect_path = Some(inspect_out.display().to_string());
                }
            }
            Ok(o) => {
                entry.error = Some(format!(
                    "fcli inspect failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ));
            }
            Err(e) => {
                entry.error = Some(format!("fcli inspect spawn failed: {e}"));
            }
        }

        let export_dir = slot.join("export");
        fs::create_dir_all(&export_dir).ok();
        let export = Command::new("fcli")
            .args([
                "file",
                "export",
                "--url",
                url,
                "--format",
                "png",
                "--scale",
                "2",
                "--output",
                &export_dir.display().to_string(),
            ])
            .current_dir(cwd)
            .output();
        match export {
            Ok(o) if o.status.success() => {
                entry.export_dir = Some(export_dir.display().to_string());
            }
            Ok(o) => {
                let msg = format!(
                    "fcli export failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                entry.error = Some(match entry.error {
                    Some(prev) => format!("{prev}; {msg}"),
                    None => msg,
                });
            }
            Err(e) => {
                entry.error = Some(format!("fcli export spawn failed: {e}"));
            }
        }
        files.push(entry);
    }

    let report = FigmaExportReport {
        version: 1,
        urls: urls.to_vec(),
        dir: figma_dir.display().to_string(),
        files,
    };
    let meta = figma_dir.join("figma.json");
    write_json_pretty(&meta, &report)?;
    Ok(Some(report))
}

pub fn session_figma_dir(session_root: &Path) -> PathBuf {
    session_root.join("figma")
}
