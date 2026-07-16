//! Deterministic ticket complexity estimation → Tier.
//!
//! All signals extracted from ticket fields alone — no I/O, no randomness.
//! Called from forge-fetch after the ticket is built and figma_urls are set.

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::ComplexityConfig;
use crate::score::Tier;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TicketComplexityBreakdown {
    pub ac: u32,
    pub description_size: u32,
    pub keywords_breadth: u32,
    pub keywords_integration: u32,
    pub keywords_risk: u32,
    pub trivial_adj: i32,
    pub story_points_score: u32,
    pub story_points_raw: Option<f64>,
    pub issue_type_adj: i32,
    pub labels_adj: i32,
    pub design: u32,
    pub discussion: u32,
    pub total_before_cap: i32,
    pub top_signals: Vec<String>,
}

/// Estimate task complexity tier from ticket metadata.
/// Returns `(tier, score_0_to_100, breakdown)`.
///
/// Arguments are intentionally flat (not `&TicketReport`) to avoid a module
/// cycle between `forge::fetch` and `forge::complexity`.
pub fn estimate_ticket_tier(
    title: &str,
    description: &str,
    labels: &[String],
    comments_count: usize,
    figma_urls_count: usize,
    fields: &serde_json::Value,
    cfg: &ComplexityConfig,
) -> (Tier, u32, TicketComplexityBreakdown) {
    let text = format!("{title} {description}");

    // AC count → bucket score
    let ac_count = count_ac(description);
    let ac = match ac_count {
        0 => 0,
        1..=2 => 8,
        3..=5 => 18,
        6..=10 => 28,
        _ => 38,
    };

    // Description word count
    let word_count = description.split_whitespace().count() as u32;
    let description_size = match word_count {
        0..=20 => 3,
        21..=80 => 8,
        81..=200 => 15,
        201..=500 => 25,
        _ => 35,
    };

    // Keyword categories — cap hits at 2 per category
    let breadth_hits = keyword_hits(&text, &cfg.breadth_keywords).min(2);
    let keywords_breadth = breadth_hits * 8;

    let integration_hits = keyword_hits(&text, &cfg.integration_keywords).min(2);
    let keywords_integration = integration_hits * 6;

    let risk_hits = keyword_hits(&text, &cfg.risk_keywords).min(2);
    let keywords_risk = risk_hits * 10;

    let trivial_hits = keyword_hits(&text, &cfg.trivial_keywords).min(2) as i32;
    let trivial_adj = -(trivial_hits * 8);

    // Story points (Jira custom fields) — dominant signal when present.
    // Buckets align with common Fibonacci scales: 1-2=S, 3-5=M, 6-8=L, 9+=XL.
    let (story_points_score, story_points_raw) =
        match story_points_from_fields(fields, &cfg.story_point_fields) {
            Some(pts) => {
                let score = match pts as u32 {
                    0 => 0,
                    1..=2 => 12,
                    3..=5 => 36,
                    6..=8 => 58,
                    _ => 80,
                };
                (score, Some(pts))
            }
            None => (0, None),
        };

    // Issue type adjustment
    let issue_type_adj = issue_type_from_fields(fields);

    // Labels
    let labels_lower: Vec<String> = labels.iter().map(|l| l.to_ascii_lowercase()).collect();
    let bump = cfg
        .bump_labels
        .iter()
        .filter(|b| {
            let b_low = b.to_ascii_lowercase();
            labels_lower.iter().any(|l| l.contains(&b_low))
        })
        .count()
        .min(1) as i32;
    let lower = cfg
        .lower_labels
        .iter()
        .filter(|b| {
            let b_low = b.to_ascii_lowercase();
            labels_lower.iter().any(|l| l.contains(&b_low))
        })
        .count()
        .min(1) as i32;
    let labels_adj = bump * 6 - lower * 6;

    // Design signal
    let design: u32 = if figma_urls_count > 0 { 5 } else { 0 };

    // Discussion
    let discussion: u32 = match comments_count {
        0 => 0,
        1..=3 => 2,
        4..=10 => 5,
        _ => 8,
    };

    // Total — story points anchor the base when present, otherwise sum all
    let total: i32 = if story_points_raw.is_some() {
        story_points_score as i32
            + keywords_breadth as i32
            + keywords_integration as i32
            + keywords_risk as i32
            + trivial_adj
            + issue_type_adj
            + labels_adj
            + design as i32
            + discussion as i32
    } else {
        ac as i32
            + description_size as i32
            + keywords_breadth as i32
            + keywords_integration as i32
            + keywords_risk as i32
            + trivial_adj
            + issue_type_adj
            + labels_adj
            + design as i32
            + discussion as i32
    };

    // Collect top contributing signals for the model prompt
    let mut signal_pairs: Vec<(i32, &str)> = Vec::new();
    if story_points_raw.is_some() {
        signal_pairs.push((story_points_score as i32, "story points"));
    } else if ac > 0 {
        signal_pairs.push((ac as i32, "AC count"));
    }
    if keywords_breadth > 0 {
        signal_pairs.push((keywords_breadth as i32, "breadth keywords"));
    }
    if keywords_integration > 0 {
        signal_pairs.push((keywords_integration as i32, "integration keywords"));
    }
    if keywords_risk > 0 {
        signal_pairs.push((keywords_risk as i32, "risk keywords"));
    }
    if description_size > 3 && story_points_raw.is_none() {
        signal_pairs.push((description_size as i32, "description size"));
    }
    if design > 0 {
        signal_pairs.push((design as i32, "Figma"));
    }
    if trivial_adj < 0 {
        signal_pairs.push((trivial_adj.abs(), "trivial keywords"));
    }
    if issue_type_adj != 0 {
        signal_pairs.push((issue_type_adj.abs(), "issue type"));
    }
    signal_pairs.sort_by(|a, b| b.0.cmp(&a.0));
    let top_signals: Vec<String> = signal_pairs
        .iter()
        .take(3)
        .map(|(_, name)| name.to_string())
        .collect();

    let score_capped = total.max(0).min(100) as u32;
    let [xs_max, s_max, m_max, l_max] = cfg.tier_thresholds;
    let tier = if score_capped <= xs_max {
        Tier::Xs
    } else if score_capped <= s_max {
        Tier::S
    } else if score_capped <= m_max {
        Tier::M
    } else if score_capped <= l_max {
        Tier::L
    } else {
        Tier::Xl
    };

    let breakdown = TicketComplexityBreakdown {
        ac,
        description_size,
        keywords_breadth,
        keywords_integration,
        keywords_risk,
        trivial_adj,
        story_points_score,
        story_points_raw,
        issue_type_adj,
        labels_adj,
        design,
        discussion,
        total_before_cap: total,
        top_signals,
    };

    (tier, score_capped, breakdown)
}

fn count_ac(description: &str) -> u32 {
    let checkboxes = description
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("- [ ]")
                || t.starts_with("* [ ]")
                || t.starts_with("+ [ ]")
                || t.starts_with("- [x]")
                || t.starts_with("* [x]")
                || t.starts_with("+ [x]")
                || t.starts_with("- [X]")
                || t.starts_with("* [X]")
                || t.starts_with("+ [X]")
        })
        .count() as u32;

    let bdd = description
        .lines()
        .filter(|l| {
            let t = l.trim().to_ascii_lowercase();
            t.starts_with("scenario:") || t.starts_with("scenario ")
        })
        .count() as u32;

    let numbered = count_numbered_under_ac_heading(description);

    checkboxes.max(bdd).max(numbered)
}

fn count_numbered_under_ac_heading(description: &str) -> u32 {
    let ac_heading = Regex::new(
        r"(?i)^#+\s*(acceptance criteria?|requirements?|criteria|ac)\b",
    )
    .ok();
    let other_heading = Regex::new(r"^#+\s+\w").ok();
    let numbered = Regex::new(r"^\d+\.\s+\S").ok();

    let mut in_ac = false;
    let mut count = 0u32;

    for line in description.lines() {
        let t = line.trim();
        if let Some(re) = &ac_heading {
            if re.is_match(t) {
                in_ac = true;
                continue;
            }
        }
        if in_ac {
            if let Some(re) = &other_heading {
                if re.is_match(t) {
                    in_ac = false;
                    continue;
                }
            }
            if let Some(re) = &numbered {
                if re.is_match(t) {
                    count += 1;
                }
            }
        }
    }
    count
}

/// Count how many distinct keywords from the list appear in `text` (case-insensitive word boundary).
fn keyword_hits(text: &str, keywords: &[String]) -> u32 {
    if keywords.is_empty() {
        return 0;
    }
    keywords
        .iter()
        .filter(|kw| {
            let escaped = regex::escape(&kw.to_ascii_lowercase());
            let pat = format!(r"(?i)\b{escaped}\b");
            Regex::new(&pat)
                .ok()
                .map_or(false, |re| re.is_match(text))
        })
        .count() as u32
}

fn story_points_from_fields(fields: &serde_json::Value, field_names: &[String]) -> Option<f64> {
    for name in field_names {
        if let Some(v) = fields.get(name.as_str()) {
            if let Some(n) = v.as_f64() {
                return Some(n);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.parse::<f64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn issue_type_from_fields(fields: &serde_json::Value) -> i32 {
    let type_str = fields
        .get("issuetype")
        .and_then(|v| v.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match type_str.as_str() {
        "epic" => 15,
        "story" | "user story" => 8,
        "task" => 0,
        "bug" => -3,
        "subtask" | "sub-task" => -8,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ComplexityConfig;
    use serde_json::json;

    fn cfg() -> ComplexityConfig {
        ComplexityConfig::default()
    }

    fn tier_for(title: &str, description: &str) -> (Tier, u32) {
        let (tier, score, _) =
            estimate_ticket_tier(title, description, &[], 0, 0, &json!({}), &cfg());
        (tier, score)
    }

    #[test]
    fn typo_fix_is_xs_or_s() {
        let (tier, score) = tier_for("Fix typo in docs", "Change 'teh' to 'the'. Simple typo.");
        assert!(
            matches!(tier, Tier::Xs | Tier::S),
            "expected XS/S got {tier} score={score}"
        );
    }

    #[test]
    fn multi_ac_refactor_is_l_or_xl() {
        let desc = "## Acceptance Criteria\n\
            - [ ] All services use the new auth module\n\
            - [ ] Unit tests pass for each service\n\
            - [ ] Integration tests cover the migration path\n\
            - [ ] No breaking changes to the public API\n\
            - [ ] Performance within 10% of baseline\n\
            - [ ] Security audit passed\n\
            Refactor the auth system across all microservices. Migrate to the new schema.";
        let (tier, score) =
            tier_for("Refactor auth across microservices", desc);
        assert!(
            matches!(tier, Tier::L | Tier::Xl),
            "expected L/XL got {tier} score={score}"
        );
    }

    #[test]
    fn story_points_dominate() {
        let fields = json!({"story_points": 8.0, "issuetype": {"name": "Story"}});
        let (tier, _score, bd) =
            estimate_ticket_tier("Add feature", "Simple addition", &[], 0, 0, &fields, &cfg());
        assert!(bd.story_points_raw.is_some());
        assert!(matches!(tier, Tier::L | Tier::Xl));
    }

    #[test]
    fn count_ac_checkbox() {
        assert_eq!(count_ac("Do the thing.\n- [ ] Step one\n- [ ] Step two\n- [x] Done"), 3);
    }

    #[test]
    fn count_ac_numbered_under_heading() {
        let desc =
            "## Acceptance Criteria\n1. User can log in\n2. User sees dashboard\n## Implementation\n3. Ignored";
        assert_eq!(count_ac(desc), 2);
    }

    #[test]
    fn count_ac_bdd() {
        let desc = "Scenario: user logs in\nScenario: user sees dashboard";
        assert_eq!(count_ac(desc), 2);
    }

    #[test]
    fn trivial_keywords_reduce_score() {
        let (_, score_normal) = tier_for("Add new feature", "Implement a new dashboard widget");
        let (_, score_trivial) = tier_for("Fix typo", "typo in wording, very minor change");
        assert!(score_trivial < score_normal, "{score_trivial} should be < {score_normal}");
    }

    #[test]
    fn figma_bump() {
        let (_, s_no_figma, _) =
            estimate_ticket_tier("UI change", "Update the button", &[], 0, 0, &json!({}), &cfg());
        let (_, s_with_figma, _) =
            estimate_ticket_tier("UI change", "Update the button", &[], 0, 1, &json!({}), &cfg());
        assert!(s_with_figma > s_no_figma);
    }
}
