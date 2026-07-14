//! Brace/heuristic symbol-range fallback, used when no tree-sitter grammar matches.

pub(crate) fn expand_brace_range(
    lines: &[&str],
    start: usize,
    end: usize,
    ctx: usize,
) -> (usize, usize) {
    let mut lo = start.saturating_sub(ctx).max(1);
    let mut hi = (end + ctx).min(lines.len());

    // Walk up to find a likely declaration / open brace
    let mut depth = 0i32;
    let mut i = start.min(lines.len());
    while i >= 1 {
        let line = lines[i - 1];
        depth += line.matches('}').count() as i32;
        depth -= line.matches('{').count() as i32;
        if looks_like_decl(line) && depth <= 0 {
            lo = i.saturating_sub(ctx).max(1);
            break;
        }
        if i == 1 {
            break;
        }
        i -= 1;
    }

    // Walk down to close braces
    depth = 0;
    for j in start..=lines.len() {
        let line = lines[j - 1];
        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;
        if depth <= 0 && j >= end {
            hi = (j + ctx).min(lines.len());
            break;
        }
    }
    (lo, hi.max(lo))
}

pub(crate) fn looks_like_decl(line: &str) -> bool {
    let t = line.trim();
    // Do not treat JSON object lines (`"key": {`) as declarations — inflates locale outlines.
    if t.starts_with('"') || t.starts_with('\'') {
        return false;
    }
    t.starts_with("fn ")
        || t.starts_with("pub fn ")
        || t.starts_with("function ")
        || t.starts_with("export function ")
        || t.starts_with("export const ")
        || t.starts_with("class ")
        || t.starts_with("pub struct ")
        || t.starts_with("impl ")
        || t.starts_with("def ")
        || t.starts_with("module ")
        || t.contains("=> {")
        || (t.ends_with('{') && !t.contains(':'))
}
