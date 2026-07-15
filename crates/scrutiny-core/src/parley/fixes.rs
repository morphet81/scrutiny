//! Structured fix outcomes written by parley agents.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::paths::write_json_pretty;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FixEntry {
    /// Thread id (`PRRT_…`) — must match `ParleyComment.id`.
    pub comment_id: String,
    pub addressed: bool,
    #[serde(default)]
    pub reply_body: String,
    #[serde(default)]
    pub explanation: String,
    #[serde(default)]
    pub code_snippet: String,
    #[serde(default)]
    pub files_touched: Vec<String>,
    /// Verifier verdict: Some(true)=fix confirmed / reply consistent,
    /// Some(false)=claimed fix does not hold, None=not verified.
    #[serde(default)]
    pub verified: Option<bool>,
    /// Verifier note on what was checked / why it failed.
    #[serde(default)]
    pub verification: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParleyFixesFile {
    #[serde(default = "default_version")]
    pub version: u32,
    pub pr_number: u64,
    pub fixes: Vec<FixEntry>,
}

fn default_version() -> u32 {
    1
}

pub fn init_fixes_file(path: &Path, pr_number: u64) -> Result<()> {
    let file = ParleyFixesFile {
        version: 1,
        pr_number,
        fixes: Vec::new(),
    };
    write_json_pretty(path, &file)
}

pub fn load_fixes(path: &Path) -> Result<ParleyFixesFile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

pub fn save_fixes(path: &Path, file: &ParleyFixesFile) -> Result<()> {
    write_json_pretty(path, file)
}

/// Merge entries by comment_id (later wins).
pub fn merge_fix_entries(file: &mut ParleyFixesFile, entries: &[FixEntry]) {
    for e in entries {
        if let Some(existing) = file.fixes.iter_mut().find(|f| f.comment_id == e.comment_id)
        {
            *existing = e.clone();
        } else {
            file.fixes.push(e.clone());
        }
    }
}

pub fn validate_fixes_complete(fixes: &ParleyFixesFile, expected_ids: &[String]) -> Result<()> {
    let have: HashSet<&str> = fixes.fixes.iter().map(|f| f.comment_id.as_str()).collect();
    let mut missing = Vec::new();
    for id in expected_ids {
        if !have.contains(id.as_str()) {
            missing.push(id.clone());
        }
    }
    if !missing.is_empty() {
        bail!(
            "parley-fixes incomplete — missing {} entry(ies): {}",
            missing.len(),
            missing.join(", ")
        );
    }
    for f in &fixes.fixes {
        if f.reply_body.trim().is_empty() && f.explanation.trim().is_empty() {
            bail!(
                "parley-fixes entry {} needs reply_body or explanation",
                f.comment_id
            );
        }
    }
    Ok(())
}

/// Build a reply body from a fix entry when reply_body empty.
pub fn compose_reply_body(fix: &FixEntry) -> String {
    if !fix.reply_body.trim().is_empty() {
        return fix.reply_body.trim().to_string();
    }
    let mut parts = Vec::new();
    if fix.addressed {
        parts.push("Addressed.".to_string());
    } else {
        parts.push("Not addressing this comment.".to_string());
    }
    if !fix.explanation.trim().is_empty() {
        parts.push(fix.explanation.trim().to_string());
    }
    if !fix.code_snippet.trim().is_empty() {
        parts.push(format!("```\n{}\n```", fix.code_snippet.trim()));
    }
    parts.join("\n\n")
}

pub fn parse_fixes_from_agent_stdout(stdout: &str) -> Vec<FixEntry> {
    // Prefer fenced json / last JSON object with "fixes" array
    if let Some(entries) = extract_fixes_json(stdout) {
        return entries;
    }
    Vec::new()
}

fn extract_fixes_json(stdout: &str) -> Option<Vec<FixEntry>> {
    // Try whole stdout
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        if let Some(arr) = pick_fixes_array(&v) {
            return Some(arr);
        }
    }
    // Cursor/Claude wrap: look for "fixes" in nested result
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        if let Some(text) = v
            .pointer("/result")
            .and_then(|r| r.as_str())
            .or_else(|| v.get("result").and_then(|r| r.as_str()))
        {
            if let Some(arr) = extract_from_text(text) {
                return Some(arr);
            }
        }
    }
    extract_from_text(stdout)
}

fn extract_from_text(text: &str) -> Option<Vec<FixEntry>> {
    // ```json ... ```
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        let after = after
            .strip_prefix("json")
            .or_else(|| after.strip_prefix("JSON"))
            .unwrap_or(after);
        let after = after.trim_start_matches('\n');
        if let Some(end) = after.find("```") {
            let block = after[..end].trim();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(block) {
                if let Some(arr) = pick_fixes_array(&v) {
                    return Some(arr);
                }
            }
        }
    }
    // Last `{` … `}` that parses
    if let Some(idx) = text.rfind('{') {
        let slice = &text[idx..];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
            return pick_fixes_array(&v);
        }
        // balanced brace walk
        let mut depth = 0i32;
        let mut end = None;
        for (i, ch) in slice.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(e) = end {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&slice[..=e]) {
                return pick_fixes_array(&v);
            }
        }
    }
    None
}

fn pick_fixes_array(v: &serde_json::Value) -> Option<Vec<FixEntry>> {
    let arr = v
        .get("fixes")
        .and_then(|f| f.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())?;
    let mut out = Vec::new();
    for item in arr {
        if let Ok(e) = serde_json::from_value::<FixEntry>(item) {
            if !e.comment_id.is_empty() {
                out.push(e);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Convenience for tests / callers that only have PathBuf.
pub fn fixes_path_display(path: &PathBuf) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_requires_all_ids() {
        let fixes = ParleyFixesFile {
            version: 1,
            pr_number: 1,
            fixes: vec![FixEntry {
                comment_id: "a".into(),
                addressed: true,
                reply_body: "done".into(),
                ..Default::default()
            }],
        };
        assert!(validate_fixes_complete(&fixes, &["a".into()]).is_ok());
        assert!(validate_fixes_complete(&fixes, &["a".into(), "b".into()]).is_err());
    }

    #[test]
    fn parse_fixes_from_fence() {
        let stdout = r#"
Done.

```json
{
  "fixes": [
    {
      "comment_id": "PRRT_1",
      "addressed": true,
      "reply_body": "Fixed",
      "explanation": "renamed",
      "code_snippet": "fn foo() {}",
      "files_touched": ["src/a.rs"]
    }
  ]
}
```
"#;
        let entries = parse_fixes_from_agent_stdout(stdout);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].comment_id, "PRRT_1");
        assert!(entries[0].addressed);
    }

    #[test]
    fn compose_when_reply_empty() {
        let e = FixEntry {
            comment_id: "x".into(),
            addressed: false,
            explanation: "Out of scope".into(),
            ..Default::default()
        };
        let body = compose_reply_body(&e);
        assert!(body.contains("Not addressing"));
        assert!(body.contains("Out of scope"));
    }
}
