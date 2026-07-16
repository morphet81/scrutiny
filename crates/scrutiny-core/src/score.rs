use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Tier {
    #[serde(rename = "XS")]
    Xs,
    S,
    M,
    L,
    #[serde(rename = "XL")]
    Xl,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Xs => "XS",
            Tier::S => "S",
            Tier::M => "M",
            Tier::L => "L",
            Tier::Xl => "XL",
        }
    }
}

impl Default for Tier {
    fn default() -> Self {
        Tier::M
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreSignals {
    pub relevant_files: u32,
    pub relevant_loc: u32,
    pub added: u32,
    pub deleted: u32,
    pub scatter: f64,
    pub blast_stub: u32,
    pub risk_path_hits: u32,
    pub layers_touched: Vec<String>,
    pub change_class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScoreBreakdown {
    pub loc: u32,
    pub files: u32,
    pub scatter: u32,
    pub blast: u32,
    pub risk: u32,
    pub layers: u32,
    pub class_adj: i32,
    pub total_before_cap: u32,
}

/// Score and bucket into tier. Pure function — easy to test.
pub fn score_tier(signals: &ScoreSignals) -> (Tier, u32) {
    let (tier, score, _) = score_tier_detailed(signals);
    (tier, score)
}

pub fn score_tier_detailed(signals: &ScoreSignals) -> (Tier, u32, ScoreBreakdown) {
    let loc = match signals.relevant_loc {
        0..=20 => 5,
        21..=80 => 15,
        81..=200 => 28,
        201..=500 => 40,
        501..=1200 => 50,
        _ => 65,
    };

    // Softened file buckets — locale fan-out no longer in score; keep multi-file modest
    let files = match signals.relevant_files {
        0..=2 => 0,
        3..=5 => 5,
        6..=12 => 10,
        13..=25 => 14,
        _ => 22,
    };

    let scatter = (signals.scatter * 12.0).round() as u32;

    let blast = match signals.blast_stub {
        0..=2 => 0,
        3..=8 => 6,
        9..=20 => 12,
        _ => 20,
    };

    let risk = signals.risk_path_hits.saturating_mul(8).min(24);

    let layers = match signals.layers_touched.len() as u32 {
        0..=1 => 0,
        2 => 5,
        3 => 8,
        _ => 14,
    };

    let mut score: u32 = loc + files + scatter + blast + risk + layers;
    let mut class_adj: i32 = 0;
    if signals.change_class == "docs" || signals.change_class == "i18n" {
        let before = score;
        score = score / 3;
        class_adj = score as i32 - before as i32;
    } else if signals.change_class == "mixed" {
        score = score.saturating_add(5);
        class_adj = 5;
    }

    let total_before_cap = score;
    let breakdown = ScoreBreakdown {
        loc,
        files,
        scatter,
        blast,
        risk,
        layers,
        class_adj,
        total_before_cap,
    };

    let tier = match score {
        0..=18 => Tier::Xs,
        19..=35 => Tier::S,
        36..=55 => Tier::M,
        56..=95 => Tier::L, // XL reserved for truly huge / high-risk fan-out
        _ => Tier::Xl,
    };

    (tier, score.min(100), breakdown)
}

/// Normalized entropy-ish scatter: 0 = all in one file, ~1 = even across many.
pub fn compute_scatter(file_locs: &[u32]) -> f64 {
    let total: u32 = file_locs.iter().sum();
    if total == 0 || file_locs.len() <= 1 {
        return 0.0;
    }
    let n = file_locs.len() as f64;
    let mut entropy = 0.0;
    for &c in file_locs {
        if c == 0 {
            continue;
        }
        let p = c as f64 / total as f64;
        entropy -= p * p.log2();
    }
    let max_entropy = n.log2();
    if max_entropy <= 0.0 {
        0.0
    } else {
        (entropy / max_entropy).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_signals() -> ScoreSignals {
        ScoreSignals {
            relevant_files: 1,
            relevant_loc: 10,
            added: 10,
            deleted: 0,
            scatter: 0.0,
            blast_stub: 0,
            risk_path_hits: 0,
            layers_touched: vec!["docs".into()],
            change_class: "docs".into(),
        }
    }

    #[test]
    fn docs_tiny_is_xs() {
        let (tier, _) = score_tier(&base_signals());
        assert_eq!(tier, Tier::Xs);
    }

    #[test]
    fn large_risky_is_high() {
        let s = ScoreSignals {
            relevant_files: 40,
            relevant_loc: 2000,
            added: 1500,
            deleted: 500,
            scatter: 0.9,
            blast_stub: 30,
            risk_path_hits: 3,
            layers_touched: vec![
                "domain".into(),
                "data".into(),
                "ui".into(),
                "routes".into(),
            ],
            change_class: "source".into(),
        };
        let (tier, score) = score_tier(&s);
        assert!(score >= 96, "score={score}");
        assert_eq!(tier, Tier::Xl);
    }

    #[test]
    fn pr2292_like_not_xl() {
        // ~17 AI-relevant files, ~800 LOC, hooks+ui, modest blast (after i18n exclusion)
        let s = ScoreSignals {
            relevant_files: 17,
            relevant_loc: 760,
            added: 750,
            deleted: 10,
            scatter: 0.7,
            blast_stub: 6, // modest aggregate after blast fix (not 40×1)
            risk_path_hits: 0,
            layers_touched: vec!["hooks".into(), "ui".into()],
            change_class: "source".into(),
        };
        let (tier, score, bd) = score_tier_detailed(&s);
        assert!(
            matches!(tier, Tier::M | Tier::L),
            "expected M/L got {tier} score={score} bd={bd:?}"
        );
        assert_ne!(tier, Tier::Xl);
    }

    #[test]
    fn scatter_one_file_zero() {
        assert_eq!(compute_scatter(&[100]), 0.0);
        assert_eq!(compute_scatter(&[]), 0.0);
    }

    #[test]
    fn scatter_even_high() {
        let s = compute_scatter(&[10, 10, 10, 10]);
        assert!(s > 0.95, "scatter={s}");
    }
}
