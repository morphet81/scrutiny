//! Declaration heuristic for outlines when no tree-sitter grammar matches.

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
