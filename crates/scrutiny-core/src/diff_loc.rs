//! Count added/deleted lines in a unified diff, skipping comment-only lines.

use std::collections::BTreeMap;

/// Per-path (added, deleted) code lines from a multi-file unified diff.
pub fn code_counts_by_path(unified: &str) -> BTreeMap<String, (u32, u32)> {
    let mut out: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    let mut path: Option<String> = None;
    let mut style = CommentStyle::None;
    let mut in_block = false;

    for line in unified.lines() {
        if let Some(p) = parse_diff_git_path(line) {
            path = Some(p.clone());
            style = CommentStyle::from_path(&p);
            in_block = false;
            out.entry(p).or_insert((0, 0));
            continue;
        }
        if line.starts_with("+++ ") || line.starts_with("--- ") || line.starts_with("@@") {
            continue;
        }
        if line.starts_with('\\') {
            continue;
        }

        let Some(p) = path.as_ref() else { continue };
        let (is_add, is_del, body) = match line.as_bytes().first() {
            Some(b'+') if !line.starts_with("+++") => (true, false, &line[1..]),
            Some(b'-') if !line.starts_with("---") => (false, true, &line[1..]),
            _ => continue,
        };

        if is_comment_only(body, style, &mut in_block) {
            continue;
        }

        let e = out.entry(p.clone()).or_insert((0, 0));
        if is_add {
            e.0 = e.0.saturating_add(1);
        }
        if is_del {
            e.1 = e.1.saturating_add(1);
        }
    }

    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommentStyle {
    None,
    CFamily, // // and /* */
    Hash,    // #
    Html,    // <!-- -->
    Sql,     // -- and /* */
}

impl CommentStyle {
    fn from_path(path: &str) -> Self {
        let lower = path.to_ascii_lowercase();
        let ext = lower.rsplit('.').next().unwrap_or("");
        match ext {
            "rs" | "ts" | "tsx" | "js" | "jsx" | "java" | "kt" | "kts" | "go" | "c" | "cc"
            | "cpp" | "cxx" | "h" | "hpp" | "cs" | "swift" | "scss" | "css" | "sass" | "less" => {
                Self::CFamily
            }
            "py" | "rb" | "toml" | "yaml" | "yml" | "sh" | "bash" | "zsh" | "r" => Self::Hash,
            "html" | "htm" | "xml" | "svg" | "vue" | "svelte" | "mdx" => Self::Html,
            "sql" => Self::Sql,
            "md" | "rst" | "txt" => Self::None, // docs excluded upstream
            _ => Self::CFamily,                 // safe default for source-like unknowns
        }
    }
}

fn parse_diff_git_path(line: &str) -> Option<String> {
    // diff --git a/path b/path  (paths may contain spaces rarely; take b/ side)
    let rest = line.strip_prefix("diff --git ")?;
    let b = rest.rfind(" b/").map(|i| &rest[i + 3..])?;
    if b.is_empty() {
        return None;
    }
    Some(b.to_string())
}

/// True if this line contributes no code (comment-only), updating block state.
fn is_comment_only(body: &str, style: CommentStyle, in_block: &mut bool) -> bool {
    if style == CommentStyle::None {
        return false;
    }

    let mut s = body;
    // Block-comment continuation / closer
    if *in_block {
        if let Some(idx) = find_block_end(s, style) {
            *in_block = false;
            s = s[idx..].trim_start();
            if s.is_empty() {
                return true;
            }
            // remainder may still be comment / code
        } else {
            return true;
        }
    }

    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false; // blank: not a comment; still counted
    }

    match style {
        CommentStyle::CFamily => {
            if let Some(rest) = strip_c_line_or_open_block(trimmed, in_block) {
                rest.is_empty()
            } else {
                false
            }
        }
        CommentStyle::Hash => {
            trimmed.starts_with('#') || {
                // trailing # comment with nothing before
                if let Some(i) = trimmed.find('#') {
                    trimmed[..i].trim().is_empty()
                } else {
                    false
                }
            }
        }
        CommentStyle::Html => html_comment_only(trimmed, in_block),
        CommentStyle::Sql => {
            if trimmed.starts_with("--") {
                true
            } else if let Some(rest) = strip_c_line_or_open_block(trimmed, in_block) {
                rest.is_empty()
            } else {
                false
            }
        }
        CommentStyle::None => false,
    }
}

fn find_block_end(s: &str, style: CommentStyle) -> Option<usize> {
    let needle = match style {
        CommentStyle::Html => "-->",
        _ => "*/",
    };
    s.find(needle).map(|i| i + needle.len())
}

/// If line is wholly a line-comment or opens a block leaving only comments, return remaining code (trimmed).
/// Sets `in_block` when `/*` (or similar) opens without close on this line.
fn strip_c_line_or_open_block(trimmed: &str, in_block: &mut bool) -> Option<String> {
    if trimmed.starts_with("//") {
        return Some(String::new());
    }

    let mut out = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'/') {
            break; // rest of line is // comment
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            // consume until */ or EOL
            let mut closed = false;
            while let Some(c2) = chars.next() {
                if c2 == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    closed = true;
                    break;
                }
            }
            if !closed {
                *in_block = true;
                break;
            }
            continue;
        }
        out.push(c);
    }
    Some(out.trim().to_string())
}

fn html_comment_only(trimmed: &str, in_block: &mut bool) -> bool {
    if trimmed.starts_with("<!--") {
        if trimmed.contains("-->") {
            let after = trimmed.split("-->").last().unwrap_or("").trim();
            return after.is_empty();
        }
        *in_block = true;
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_line_comments() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 fn main() {
+    // todo
+    let x = 1;
 }
";
        let m = code_counts_by_path(diff);
        assert_eq!(m.get("src/a.rs"), Some(&(1, 0)));
    }

    #[test]
    fn skips_block_comment_lines() {
        let diff = "\
diff --git a/src/a.ts b/src/a.ts
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,2 +1,5 @@
 export const a = 1;
+/* dead
+ * block
+ */
+export const b = 2;
";
        let m = code_counts_by_path(diff);
        assert_eq!(m.get("src/a.ts"), Some(&(1, 0)));
    }

    #[test]
    fn counts_code_with_trailing_comment() {
        let diff = "\
diff --git a/x.go b/x.go
--- a/x.go
+++ b/x.go
@@ -0,0 +1 @@
+package main // ok
";
        let m = code_counts_by_path(diff);
        assert_eq!(m.get("x.go"), Some(&(1, 0)));
    }

    #[test]
    fn hash_comments_py() {
        let diff = "\
diff --git a/a.py b/a.py
--- a/a.py
+++ b/a.py
@@ -0,0 +1,2 @@
+# noop
+x = 1
";
        let m = code_counts_by_path(diff);
        assert_eq!(m.get("a.py"), Some(&(1, 0)));
    }
}
