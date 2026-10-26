//! Full-file symbol outline (always present, even when a file's body is dropped) +
//! symbol-region construction. Prefers tree-sitter; falls back to the brace heuristic.

use crate::config::PackConfig;
use crate::treesitter;

use super::budget::Region;
use super::slice::looks_like_decl;
use super::OutlineEntry;

/// Inclusive ranges of added lines only. Hunk context is not a change.
pub(crate) fn changed_ranges(diff: &str) -> Vec<(usize, usize)> {
    super::hunk::added_line_ranges(diff)
}

pub(crate) fn build_outline(
    path: &str,
    head_text: &str,
    ranges: &[(usize, usize)],
) -> Vec<OutlineEntry> {
    let mut entries = match treesitter::outline(path, head_text) {
        Some(decls) => decls
            .into_iter()
            .map(|d| OutlineEntry {
                kind: d.kind,
                name: d.name,
                signature: d.signature,
                start_line: d.start,
                end_line: d.end,
                in_diff: false,
            })
            .collect::<Vec<_>>(),
        None => fallback_outline(head_text),
    };
    for e in &mut entries {
        e.in_diff = ranges
            .iter()
            .any(|&(lo, hi)| e.start_line <= hi && e.end_line >= lo);
    }
    entries
}

fn fallback_outline(text: &str) -> Vec<OutlineEntry> {
    let lines: Vec<&str> = text.lines().collect();
    let decl_lines: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| looks_like_decl(l))
        .map(|(i, _)| i + 1)
        .collect();
    let mut out = Vec::new();
    for (idx, &start) in decl_lines.iter().enumerate() {
        let end = decl_lines
            .get(idx + 1)
            .map(|n| n.saturating_sub(1))
            .unwrap_or(lines.len())
            .max(start);
        let sig = lines[start - 1].trim().to_string();
        out.push(OutlineEntry {
            kind: "decl".into(),
            name: decl_name(&sig),
            signature: sig,
            start_line: start,
            end_line: end,
            in_diff: false,
        });
    }
    out
}

fn decl_name(sig: &str) -> String {
    // Best-effort: token after the first keyword, stripped of punctuation.
    sig.split_whitespace()
        .nth(1)
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
        .to_string()
}

pub(crate) fn build_regions(
    path: &str,
    head_text: &str,
    outline: &[OutlineEntry],
    ranges: &[(usize, usize)],
    cfg: &PackConfig,
) -> Vec<Region> {
    // Outline and context padding stay available to callers; the slice itself
    // is only the added lines. A whole symbol would put unchanged lines in front
    // of the reviewer.
    let _ = (outline, cfg);
    let lines: Vec<&str> = head_text.lines().collect();
    let mut regions: Vec<Region> = ranges
        .iter()
        .copied()
        .filter(|&(lo, hi)| lo > 0 && hi >= lo)
        .map(|(lo, hi)| region_from(path, &lines, lo, hi))
        .filter(|r| !r.content.is_empty())
        .collect();
    regions.sort_by_key(|r| (r.end.saturating_sub(r.start), r.start));
    regions
}

fn region_from(path: &str, lines: &[&str], start: usize, end: usize) -> Region {
    let lo = start.max(1);
    let hi = end.min(lines.len()).max(lo);
    let content = lines
        .get(lo - 1..hi)
        .map(|c| c.join("\n"))
        .unwrap_or_default();
    Region {
        label: format!("{path}:{lo}-{hi}"),
        start: lo,
        end: hi,
        content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_in_diff_by_overlap() {
        let src = "fn a() {\n  1\n}\nfn b() {\n  2\n}\n";
        let o = build_outline("x.rs", src, &[(5, 5)]);
        let a = o.iter().find(|e| e.name == "a").unwrap();
        let b = o.iter().find(|e| e.name == "b").unwrap();
        assert!(!a.in_diff);
        assert!(b.in_diff);
    }

    #[test]
    fn regions_are_changed_lines_not_the_whole_symbol() {
        let src = "fn b() {\n  keep\n  changed\n  keep\n}\n";
        let o = build_outline("x.rs", src, &[(3, 3)]);
        let r = build_regions("x.rs", src, &o, &[(3, 3)], &PackConfig::default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].content, "  changed");
    }

    #[test]
    fn changed_ranges_skip_context() {
        let diff = "@@ -1,4 +1,4 @@\n keep\n-old\n+new\n keep\n";
        assert_eq!(changed_ranges(diff), vec![(2, 2)]);
    }

    #[test]
    fn fallback_when_no_grammar() {
        let src = "def foo\n  1\nend\n";
        let o = build_outline("x.unknownext", src, &[(1, 1)]);
        assert!(!o.is_empty());
        assert!(o[0].in_diff);
    }
}
