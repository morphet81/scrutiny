//! Forge LOC budget rules, estimate parsing, and pre-implement gate helpers.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::config::{default_loc_exclude_extensions, ForgeConfig};
use crate::taxonomy::{classify_path, PathKind};

/// Counting rules for forge LOC budget (estimate + future post-count).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeLocRules {
    pub exclude_test: bool,
    pub exclude_doc: bool,
    pub exclude_comments: bool,
    /// Lowercase extensions without leading dots.
    pub exclude_extensions: Vec<String>,
}

impl Default for ForgeLocRules {
    fn default() -> Self {
        Self {
            exclude_test: true,
            exclude_doc: true,
            exclude_comments: true,
            exclude_extensions: default_loc_exclude_extensions(),
        }
    }
}

impl ForgeLocRules {
    pub fn from_forge(f: &ForgeConfig) -> Self {
        Self {
            exclude_test: f.loc_exclude_test,
            exclude_doc: f.loc_exclude_doc,
            exclude_comments: f.loc_exclude_comments,
            exclude_extensions: f
                .loc_exclude_extensions
                .iter()
                .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
        }
    }

    /// Bullet list for the estimate-agent prompt.
    pub fn prompt_bullets(&self) -> String {
        let mut lines = vec![
            "- Metric: additions + deletions (PR-style changed LOC; updates count as both)."
                .to_string(),
            format!(
                "- Exclude test paths: {}",
                if self.exclude_test { "yes" } else { "no" }
            ),
            format!(
                "- Exclude doc paths: {}",
                if self.exclude_doc { "yes" } else { "no" }
            ),
            format!(
                "- Exclude comment-only lines: {}",
                if self.exclude_comments { "yes" } else { "no" }
            ),
        ];
        if self.exclude_extensions.is_empty() {
            lines.push("- Exclude extensions: (none)".into());
        } else {
            lines.push(format!(
                "- Exclude extensions: {}",
                self.exclude_extensions.join(", ")
            ));
        }
        lines.join("\n")
    }
}

/// True when this path should not contribute to LOC under `rules`.
pub fn path_excluded_from_loc(path: &str, rules: &ForgeLocRules) -> bool {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    if let Some(ext) = file_extension(&lower) {
        if rules.exclude_extensions.iter().any(|e| e == ext) {
            return true;
        }
    }
    match classify_path(path) {
        PathKind::Test if rules.exclude_test => true,
        PathKind::Doc if rules.exclude_doc => true,
        _ => false,
    }
}

fn file_extension(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.starts_with('.') && !name[1..].contains('.') {
        return None; // e.g. ".gitignore"
    }
    let ext = name.rsplit('.').next()?;
    if ext.is_empty() || ext == name {
        return None;
    }
    Some(ext)
}

/// Agent estimate of future PR LOC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocEstimate {
    pub estimated_loc: u32,
    /// 0.0–1.0 when known.
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub confidence_label: Option<String>,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub breakdown: BTreeMap<String, u32>,
}

impl LocEstimate {
    /// Resolved confidence in 0.0–1.0 (label fallback: low=0.35, medium=0.6, high=0.85).
    pub fn confidence_score(&self) -> f64 {
        if let Some(c) = self.confidence {
            return c.clamp(0.0, 1.0);
        }
        match self
            .confidence_label
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("high") => 0.85,
            Some("medium") | Some("med") => 0.6,
            Some("low") => 0.35,
            _ => 0.5,
        }
    }

    pub fn confidence_pct(&self) -> u32 {
        (self.confidence_score() * 100.0).round() as u32
    }

    pub fn display_label(&self) -> String {
        if let Some(l) = self.confidence_label.as_deref().map(str::trim) {
            if !l.is_empty() {
                return l.to_string();
            }
        }
        let s = self.confidence_score();
        if s >= 0.75 {
            "high".into()
        } else if s >= 0.5 {
            "medium".into()
        } else {
            "low".into()
        }
    }
}

/// Outcome of comparing estimate to `max_loc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocGateDecision {
    /// Under budget — continue.
    Proceed,
    /// Over budget and non-interactive — stop.
    Abort,
    /// Over budget and interactive — ask user.
    Ask,
}

/// Pure gate: under budget → Proceed; over → Ask if interactive else Abort.
pub fn decide_loc_gate(estimated: u32, max_loc: u32, interactive: bool) -> LocGateDecision {
    if estimated <= max_loc {
        LocGateDecision::Proceed
    } else if interactive {
        LocGateDecision::Ask
    } else {
        LocGateDecision::Abort
    }
}

/// Parse estimate JSON from file contents or agent stdout.
pub fn parse_loc_estimate(raw: &str) -> Result<LocEstimate> {
    let text = extract_json_payload(raw)?;
    let mut v: serde_json::Value =
        serde_json::from_str(&text).context("parse loc-estimate JSON")?;
    if v.get("estimated_loc").is_none() {
        if let Some(r) = v.get("result").and_then(|x| x.as_str()) {
            return parse_loc_estimate(r);
        }
        if let Some(r) = v.get("structured_output") {
            return parse_loc_estimate(&r.to_string());
        }
    }
    normalize_confidence_field(&mut v);
    let mut est: LocEstimate = serde_json::from_value(v).context("deserialize LocEstimate")?;
    if est.confidence.is_none() {
        if let Some(label) = est.confidence_label.as_deref() {
            if let Some(score) = label_to_confidence(label) {
                est.confidence = Some(score);
            }
        }
    }
    Ok(est)
}

fn label_to_confidence(label: &str) -> Option<f64> {
    match label.trim().to_ascii_lowercase().as_str() {
        "high" => Some(0.85),
        "medium" | "med" => Some(0.6),
        "low" => Some(0.35),
        _ => None,
    }
}

fn normalize_confidence_field(v: &mut serde_json::Value) {
    let Some(c) = v.get("confidence").cloned() else {
        return;
    };
    if let Some(s) = c.as_str() {
        if let Some(score) = label_to_confidence(s) {
            v["confidence"] = serde_json::json!(score);
            if v.get("confidence_label").and_then(|x| x.as_str()).is_none() {
                let lower = s.trim().to_ascii_lowercase();
                let label = if lower == "med" {
                    "medium"
                } else {
                    lower.as_str()
                };
                v["confidence_label"] = serde_json::json!(label);
            }
            return;
        }
        if let Ok(n) = s.trim().parse::<f64>() {
            let score = if n > 1.0 && n <= 100.0 { n / 100.0 } else { n };
            v["confidence"] = serde_json::json!(score.clamp(0.0, 1.0));
            return;
        }
        v.as_object_mut().map(|o| o.remove("confidence"));
        return;
    }
    if let Some(n) = c.as_f64() {
        if n > 1.0 && n <= 100.0 {
            v["confidence"] = serde_json::json!(n / 100.0);
        }
    }
}

fn extract_json_payload(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Ok(trimmed.to_string());
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = after
            .strip_prefix("json")
            .or_else(|| after.strip_prefix("JSON"))
            .unwrap_or(after);
        if let Some(end) = after.find("```") {
            return Ok(after[..end].trim().to_string());
        }
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(r) = v.get("result").and_then(|x| x.as_str()) {
            return extract_json_payload(r);
        }
        return Ok(trimmed.to_string());
    }
    // Last resort: find first `{` … last `}`.
    if let (Some(i), Some(j)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if j > i {
            return Ok(trimmed[i..=j].to_string());
        }
    }
    bail!("could not extract JSON from loc-estimate agent output");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_test_doc_and_ext() {
        let rules = ForgeLocRules::default();
        assert!(path_excluded_from_loc("src/foo.test.ts", &rules));
        assert!(path_excluded_from_loc("docs/guide.md", &rules));
        assert!(path_excluded_from_loc("assets/logo.png", &rules));
        assert!(!path_excluded_from_loc("src/foo.rs", &rules));
    }

    #[test]
    fn can_include_test_when_flag_off() {
        let mut rules = ForgeLocRules::default();
        rules.exclude_test = false;
        assert!(!path_excluded_from_loc("src/foo.test.ts", &rules));
        assert!(path_excluded_from_loc("icon.webp", &rules));
    }

    #[test]
    fn extension_dot_tolerant() {
        let mut rules = ForgeLocRules::default();
        rules.exclude_extensions = vec![".PNG".into()];
        // from_forge normalizes; path check lowercases path + compares to stored
        rules.exclude_extensions = vec!["png".into()];
        assert!(path_excluded_from_loc("A.PNG", &rules));
    }

    #[test]
    fn parse_estimate_raw_and_fenced() {
        let raw = r#"{"estimated_loc": 120, "confidence": 0.8, "rationale": "small"}"#;
        let e = parse_loc_estimate(raw).unwrap();
        assert_eq!(e.estimated_loc, 120);
        assert!((e.confidence_score() - 0.8).abs() < 1e-9);

        let fenced = "```json\n{\"estimated_loc\": 50, \"confidence_label\": \"low\"}\n```";
        let e2 = parse_loc_estimate(fenced).unwrap();
        assert_eq!(e2.estimated_loc, 50);
        assert!((e2.confidence_score() - 0.35).abs() < 1e-9);

        let labeled = r#"{"estimated_loc": 10, "confidence": "high"}"#;
        let e3 = parse_loc_estimate(labeled).unwrap();
        assert!((e3.confidence_score() - 0.85).abs() < 1e-9);
        assert_eq!(e3.display_label(), "high");

        let pct = r#"{"estimated_loc": 10, "confidence": 72}"#;
        let e4 = parse_loc_estimate(pct).unwrap();
        assert!((e4.confidence_score() - 0.72).abs() < 1e-9);
    }

    #[test]
    fn decide_gate() {
        assert_eq!(decide_loc_gate(100, 200, true), LocGateDecision::Proceed);
        assert_eq!(decide_loc_gate(200, 200, false), LocGateDecision::Proceed);
        assert_eq!(decide_loc_gate(201, 200, true), LocGateDecision::Ask);
        assert_eq!(decide_loc_gate(201, 200, false), LocGateDecision::Abort);
    }

    #[test]
    fn prompt_bullets_include_flags() {
        let b = ForgeLocRules::default().prompt_bullets();
        assert!(b.contains("Exclude test paths: yes"));
        assert!(b.contains("Exclude doc paths: yes"));
        assert!(b.contains("Exclude comment-only lines: yes"));
        assert!(b.contains("png"));
    }
}
