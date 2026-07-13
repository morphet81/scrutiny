use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};

pub fn temp_dir() -> PathBuf {
    std::env::var_os("TMPDIR")
        .or_else(|| std::env::var_os("TMP"))
        .or_else(|| std::env::var_os("TEMP"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

pub fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

pub fn temp_artifact_path(repo: &str, branch: &str, kind: &str) -> PathBuf {
    let ts = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let nonce: u32 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
        .unwrap_or(0);
    let name = format!(
        "scrutiny-{}-{}-{}-{:08x}-{}.json",
        slug(repo),
        slug(branch),
        ts,
        nonce,
        kind
    );
    temp_dir().join(name)
}

pub fn write_json_pretty(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let text = serde_json::to_string_pretty(value).context("serialize json")?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
