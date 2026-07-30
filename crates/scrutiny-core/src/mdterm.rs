//! Minimal markdown → ANSI renderer for terminal display (headings, bold,
//! inline code, bullets, code fences). Colors auto-disable off a TTY; markdown
//! markers are stripped regardless, so the output is clean plain text too.

use console::Style;

/// Render markdown to a terminal-friendly string.
pub fn render_markdown(src: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;

    for raw in src.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push(Style::new().dim().apply_to(line).to_string());
            continue;
        }
        if in_fence {
            out.push(Style::new().dim().apply_to(line).to_string());
            continue;
        }

        // Horizontal rule
        if is_hr(trimmed) {
            out.push(Style::new().dim().apply_to("─".repeat(40)).to_string());
            continue;
        }

        // Headings
        if let Some((level, text)) = heading(trimmed) {
            let style = match level {
                1 => Style::new().cyan().bold().underlined(),
                2 => Style::new().green().bold(),
                3 => Style::new().yellow().bold(),
                _ => Style::new().bold(),
            };
            out.push(style.apply_to(inline(text)).to_string());
            continue;
        }

        // Blockquote
        if let Some(rest) = trimmed.strip_prefix("> ") {
            let body = Style::new().dim().apply_to(inline(rest)).to_string();
            out.push(format!("{} {}", Style::new().dim().apply_to("│"), body));
            continue;
        }

        // Bullets
        if let Some(rest) = bullet(trimmed) {
            let indent = &line[..line.len() - trimmed.len()];
            let dot = Style::new().cyan().apply_to("•");
            out.push(format!("{indent}  {dot} {}", inline(rest)));
            continue;
        }

        out.push(inline(line));
    }

    out.join("\n")
}

fn is_hr(s: &str) -> bool {
    let s = s.replace(' ', "");
    s.len() >= 3
        && (s.chars().all(|c| c == '-')
            || s.chars().all(|c| c == '*')
            || s.chars().all(|c| c == '_'))
}

fn heading(s: &str) -> Option<(usize, &str)> {
    if !s.starts_with('#') {
        return None;
    }
    let hashes = s.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &s[hashes..];
    let text = rest.strip_prefix(' ')?;
    Some((hashes, text))
}

fn bullet(s: &str) -> Option<&str> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(m) {
            return Some(rest);
        }
    }
    None
}

/// Inline styling: `**bold**`/`__bold__`, `*em*`/`_em_`, `` `code` ``.
fn inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        // inline code
        if chars[i] == '`' {
            if let Some(end) = find_from(&chars, i + 1, '`') {
                let code: String = chars[i + 1..end].iter().collect();
                out.push_str(&Style::new().cyan().apply_to(code).to_string());
                i = end + 1;
                continue;
            }
        }
        // bold: ** or __
        if i + 1 < chars.len() && (chars[i] == '*' || chars[i] == '_') && chars[i + 1] == chars[i] {
            let marker = chars[i];
            if let Some(end) = find_double(&chars, i + 2, marker) {
                let inner: String = chars[i + 2..end].iter().collect();
                out.push_str(&Style::new().bold().apply_to(inner).to_string());
                i = end + 2;
                continue;
            }
        }
        // italic: single * or _
        if (chars[i] == '*' || chars[i] == '_') && i + 1 < chars.len() && chars[i + 1] != chars[i] {
            let marker = chars[i];
            if let Some(end) = find_from(&chars, i + 1, marker) {
                let inner: String = chars[i + 1..end].iter().collect();
                out.push_str(&Style::new().italic().apply_to(inner).to_string());
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn find_from(chars: &[char], start: usize, target: char) -> Option<usize> {
    (start..chars.len()).find(|&j| chars[j] == target)
}

fn find_double(chars: &[char], start: usize, marker: char) -> Option<usize> {
    let mut j = start;
    while j + 1 < chars.len() {
        if chars[j] == marker && chars[j + 1] == marker {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Usable stderr width. Falls back to 100 when the size is unknown (piped,
/// CI), and stays inside 60..=200 so very narrow or very wide terminals still
/// produce readable blocks.
pub fn term_cols() -> usize {
    let cols = console::Term::stderr()
        .size_checked()
        .map(|(_, c)| c as usize)
        .unwrap_or(100);
    cols.clamp(60, 200)
}

/// Word-wrap `text` to `cols`, prefixing the first line with `first_prefix` and
/// every continuation line with `cont_prefix`. Existing newlines are hard
/// breaks. Words longer than the available width are left intact (never split)
/// so paths and identifiers stay copy-pasteable.
pub fn wrap_indent(text: &str, first_prefix: &str, cont_prefix: &str, cols: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut first_line = true;

    for para in text.split('\n') {
        let mut cur = String::new();
        let mut cur_prefix = if first_line {
            first_prefix
        } else {
            cont_prefix
        };
        let mut has_word = false;

        for word in para.split_whitespace() {
            let width = cols.saturating_sub(cur_prefix.chars().count()).max(1);
            let next_len = if has_word {
                cur.chars().count() + 1 + word.chars().count()
            } else {
                word.chars().count()
            };
            if has_word && next_len > width {
                out.push(format!("{cur_prefix}{cur}"));
                cur_prefix = cont_prefix;
                cur.clear();
                cur.push_str(word);
            } else {
                if has_word {
                    cur.push(' ');
                }
                cur.push_str(word);
                has_word = true;
            }
        }
        if has_word {
            out.push(format!("{cur_prefix}{cur}"));
        } else {
            out.push(cur_prefix.trim_end().to_string());
        }
        first_line = false;
    }

    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> String {
        console::set_colors_enabled(false);
        render_markdown(s)
    }

    #[test]
    fn strips_heading_marker_keeps_text() {
        let out = plain("# Test Plan");
        assert!(out.contains("Test Plan"));
        assert!(!out.contains('#'));
    }

    #[test]
    fn strips_bold_and_code_markers() {
        let out = plain("A **bold** and `code` here");
        assert!(out.contains("bold"));
        assert!(out.contains("code"));
        assert!(!out.contains("**"));
        assert!(!out.contains('`'));
    }

    #[test]
    fn bullets_become_dots() {
        let out = plain("- first\n- second");
        assert!(out.contains("• first"));
        assert!(out.contains("• second"));
        assert!(!out.contains("- first"));
    }

    #[test]
    fn fence_content_preserved() {
        let out = plain("```\nlet x = 1;\n```");
        assert!(out.contains("let x = 1;"));
    }

    #[test]
    fn table_line_passes_through() {
        let out = plain("| a | b |");
        assert!(out.contains("| a | b |"));
    }

    #[test]
    fn term_cols_stays_in_range() {
        let c = term_cols();
        assert!((60..=200).contains(&c), "got {c}");
    }

    #[test]
    fn wrap_indent_hangs_continuation_and_respects_width() {
        let out = wrap_indent("aaa bbb ccc ddd eee", "  A) ", "     ", 14);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() > 1);
        assert!(lines[0].starts_with("  A) "));
        for l in &lines[1..] {
            assert!(l.starts_with("     "), "bad continuation: {l:?}");
        }
        for l in &lines {
            assert!(l.chars().count() <= 14, "too wide: {l:?}");
        }
    }

    #[test]
    fn wrap_indent_keeps_full_text() {
        let text = "Widen ReservationsController.fetchReservations to ReservationsListParams. \
                    Preferred: merge become reachable and match hook type.";
        let out = wrap_indent(text, "  A) ", "     ", 40);
        let flat: String = out
            .lines()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ")
            .replace("A) ", "");
        assert_eq!(flat.trim(), text);
    }

    #[test]
    fn wrap_indent_never_splits_long_word() {
        let out = wrap_indent(
            "src/data/hooks/use-reservations-with-a-very-long-name.ts",
            "  ",
            "  ",
            20,
        );
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("use-reservations-with-a-very-long-name.ts"));
    }

    #[test]
    fn wrap_indent_keeps_hard_breaks() {
        let out = wrap_indent("one\n\ntwo", "  ", "  ", 40);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines, vec!["  one", "", "  two"]);
    }
}
