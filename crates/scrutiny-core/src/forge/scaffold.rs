//! Deterministic guesses for the forge scaffolding step: conventional-commit
//! prefix, branch name, PR title, commit subject — all derived from the ticket.
//! The host asks the user only to *confirm* these; it never asks the AI to
//! author them.

use crate::forge::fetch::TicketReport;
use crate::paths::slug;

pub const PREFIXES: [&str; 7] = ["feat", "fix", "docs", "refactor", "perf", "test", "chore"];

const TITLE_MAX_WORDS: usize = 6;
const SLUG_MAX_LEN: usize = 40;
const SUBJECT_MAX_LEN: usize = 72;

/// Guess a conventional-commit prefix: ticket type → labels → title keywords → `feat`.
pub fn guess_prefix(ticket: &TicketReport) -> &'static str {
    if let Some(p) = prefix_from_type(ticket) {
        return p;
    }
    if let Some(p) = prefix_from_labels(&ticket.labels) {
        return p;
    }
    if let Some(p) = prefix_from_text(&ticket.title) {
        return p;
    }
    "feat"
}

fn prefix_from_type(ticket: &TicketReport) -> Option<&'static str> {
    let t = ticket
        .fields
        .pointer("/issuetype/name")
        .and_then(|v| v.as_str())?
        .to_ascii_lowercase();
    Some(match t.as_str() {
        s if s.contains("bug") => "fix",
        s if s.contains("story")
            || s.contains("task")
            || s.contains("epic")
            || s.contains("feature")
            || s.contains("improvement") =>
        {
            "feat"
        }
        _ => return None,
    })
}

fn prefix_from_labels(labels: &[String]) -> Option<&'static str> {
    for raw in labels {
        let l = raw.to_ascii_lowercase();
        let hit = match l.as_str() {
            s if s.contains("bug") => Some("fix"),
            s if s.contains("documentation") || s == "docs" => Some("docs"),
            s if s.contains("enhancement") || s.contains("feature") => Some("feat"),
            s if s.contains("refactor") => Some("refactor"),
            s if s.contains("performance") || s.contains("perf") => Some("perf"),
            s if s.contains("test") => Some("test"),
            s if s.contains("chore") => Some("chore"),
            _ => None,
        };
        if hit.is_some() {
            return hit;
        }
    }
    None
}

fn prefix_from_text(title: &str) -> Option<&'static str> {
    let t = title.to_ascii_lowercase();
    let first = t.split_whitespace().next().unwrap_or("");
    // Leading verb is the strongest signal.
    match first {
        "fix" | "fixes" | "fixed" | "bugfix" => return Some("fix"),
        "refactor" => return Some("refactor"),
        "add" | "adds" | "implement" | "implements" | "support" | "create" | "introduce" => {
            return Some("feat")
        }
        "test" | "tests" => return Some("test"),
        _ => {}
    }
    if t.contains("bug") || t.contains(" fix") {
        Some("fix")
    } else if t.contains("refactor") {
        Some("refactor")
    } else if t.contains("document") || t.contains("readme") {
        Some("docs")
    } else if t.contains("optimi") || t.contains("performance") {
        Some("perf")
    } else {
        None
    }
}

/// `<prefix>/<id>-<title-slug>` (inline tickets drop the id part).
pub fn branch_name(ticket: &TicketReport, prefix: &str) -> String {
    let title_slug = title_slug(&ticket.title);
    let id = slug(ticket.id.trim_start_matches('#')).to_ascii_lowercase();
    if id.is_empty() || id == "inline" {
        format!("{prefix}/{title_slug}")
    } else {
        let stem = format!("{id}-{title_slug}");
        format!("{prefix}/{}", stem.trim_matches('-'))
    }
}

fn title_slug(title: &str) -> String {
    let words: Vec<&str> = title.split_whitespace().take(TITLE_MAX_WORDS).collect();
    let mut s = slug(&words.join("-")).to_ascii_lowercase();
    if s.len() > SLUG_MAX_LEN {
        s.truncate(SLUG_MAX_LEN);
    }
    s.trim_matches('-').to_string()
}

/// Editable default for the PR title.
pub fn guess_pr_title(ticket: &TicketReport, prefix: &str) -> String {
    format!("{prefix}: {}", ticket.title.trim())
}

/// Editable default for the PR description: ticket body + a trailing ticket ref.
pub fn guess_pr_body(ticket: &TicketReport) -> String {
    let mut body = ticket.description.trim().to_string();
    if let Some(url) = ticket.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&format!("Refs: {url}"));
    }
    body
}

/// Editable default for the one-line commit subject (≤72 chars).
pub fn guess_commit_subject(ticket: &TicketReport, prefix: &str) -> String {
    let mut s = format!("{prefix}: {}", ticket.title.trim());
    if s.chars().count() > SUBJECT_MAX_LEN {
        s = s.chars().take(SUBJECT_MAX_LEN - 1).collect::<String>() + "…";
    }
    s
}

/// Reorder `PREFIXES` so the guess is first — for the confirmation Select.
pub fn prefix_choices(guess: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::with_capacity(PREFIXES.len());
    if let Some(&g) = PREFIXES.iter().find(|&&p| p == guess) {
        out.push(g);
    }
    for &p in PREFIXES.iter() {
        if p != guess {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ticket(source: &str, id: &str, title: &str, labels: &[&str], fields: serde_json::Value) -> TicketReport {
        TicketReport {
            version: 1,
            source: source.into(),
            id: id.into(),
            url: None,
            title: title.into(),
            description: String::new(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            comments: vec![],
            attachments_dir: None,
            figma_urls: vec![],
            figma_dir: None,
            fields,
            raw_path: None,
            fetched_at: String::new(),
            suggested_forge: crate::config::SuggestedForge::default(),
        }
    }

    #[test]
    fn pr_body_appends_ref_when_url_present() {
        let mut t = ticket("jira", "PROJ-9", "Title", &[], json!({}));
        t.description = "Implement the thing.".into();
        t.url = Some("https://jira/PROJ-9".into());
        assert_eq!(
            guess_pr_body(&t),
            "Implement the thing.\n\nRefs: https://jira/PROJ-9"
        );
    }

    #[test]
    fn pr_body_without_url_is_description_only() {
        let mut t = ticket("inline", "inline", "Title", &[], json!({}));
        t.description = "Just a description.".into();
        assert_eq!(guess_pr_body(&t), "Just a description.");
    }

    #[test]
    fn pr_body_url_only_when_no_description() {
        let mut t = ticket("jira", "PROJ-9", "Title", &[], json!({}));
        t.url = Some("https://jira/PROJ-9".into());
        assert_eq!(guess_pr_body(&t), "Refs: https://jira/PROJ-9");
    }

    #[test]
    fn prefix_from_jira_type() {
        let t = ticket(
            "jira",
            "PROJ-1",
            "Whatever",
            &[],
            json!({"issuetype": {"name": "Bug"}}),
        );
        assert_eq!(guess_prefix(&t), "fix");
        let t = ticket(
            "jira",
            "PROJ-2",
            "Whatever",
            &[],
            json!({"issuetype": {"name": "Story"}}),
        );
        assert_eq!(guess_prefix(&t), "feat");
    }

    #[test]
    fn prefix_from_github_labels() {
        let t = ticket("github", "#5", "Something", &["bug", "p1"], json!({}));
        assert_eq!(guess_prefix(&t), "fix");
        let t = ticket("github", "#6", "Something", &["enhancement"], json!({}));
        assert_eq!(guess_prefix(&t), "feat");
    }

    #[test]
    fn prefix_from_title_keywords() {
        let t = ticket("inline", "inline", "Fix the broken login", &[], json!({}));
        assert_eq!(guess_prefix(&t), "fix");
        let t = ticket("inline", "inline", "Add a dark mode toggle", &[], json!({}));
        assert_eq!(guess_prefix(&t), "feat");
        let t = ticket("inline", "inline", "Refactor the parser", &[], json!({}));
        assert_eq!(guess_prefix(&t), "refactor");
    }

    #[test]
    fn prefix_defaults_to_feat() {
        let t = ticket("inline", "inline", "Miscellaneous stuff", &[], json!({}));
        assert_eq!(guess_prefix(&t), "feat");
    }

    #[test]
    fn branch_name_with_id() {
        let t = ticket("jira", "PROJ-123", "Add a widget to the panel now please", &[], json!({}));
        assert_eq!(branch_name(&t, "feat"), "feat/proj-123-add-a-widget-to-the-panel");
    }

    #[test]
    fn branch_name_github_strips_hash() {
        let t = ticket("github", "#42", "Fix bug", &[], json!({}));
        assert_eq!(branch_name(&t, "fix"), "fix/42-fix-bug");
    }

    #[test]
    fn branch_name_inline_drops_id() {
        let t = ticket("inline", "inline", "Add a thing", &[], json!({}));
        assert_eq!(branch_name(&t, "feat"), "feat/add-a-thing");
    }

    #[test]
    fn title_and_subject_shape() {
        let t = ticket("jira", "PROJ-1", "Add search", &[], json!({}));
        assert_eq!(guess_pr_title(&t, "feat"), "feat: Add search");
        assert_eq!(guess_commit_subject(&t, "feat"), "feat: Add search");
    }

    #[test]
    fn subject_truncates() {
        let long = "x".repeat(200);
        let t = ticket("jira", "PROJ-1", &long, &[], json!({}));
        assert!(guess_commit_subject(&t, "feat").chars().count() <= SUBJECT_MAX_LEN);
    }

    #[test]
    fn choices_put_guess_first() {
        let c = prefix_choices("chore");
        assert_eq!(c[0], "chore");
        assert_eq!(c.len(), PREFIXES.len());
    }
}
