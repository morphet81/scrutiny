/// Inclusive new-file ranges of **added** (`+`) lines only. Context and deletions
/// are not included, and a context line between two additions splits the range.
pub(crate) fn added_line_ranges(diff: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut new_line: usize = 0;
    let mut run_lo: Option<usize> = None;
    let mut run_hi: usize = 0;
    let flush = |run_lo: &mut Option<usize>, run_hi: usize, out: &mut Vec<(usize, usize)>| {
        if let Some(lo) = run_lo.take() {
            if lo > 0 {
                out.push((lo, run_hi.max(lo)));
            }
        }
    };
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            flush(&mut run_lo, run_hi, &mut out);
            new_line = hunk_new_start(rest).saturating_sub(1);
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") || diff_noise(line) {
            continue;
        }
        if line.starts_with('+') {
            new_line += 1;
            if run_lo.is_none() {
                run_lo = Some(new_line);
            }
            run_hi = new_line;
        } else if line.starts_with('-') {
            flush(&mut run_lo, run_hi, &mut out);
        } else {
            flush(&mut run_lo, run_hi, &mut out);
            new_line += 1;
        }
    }
    flush(&mut run_lo, run_hi, &mut out);
    out
}

fn hunk_new_start(header_rest: &str) -> usize {
    header_rest
        .split('+')
        .nth(1)
        .and_then(|plus| {
            plus.split(|c: char| c == ',' || c == ' ')
                .next()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0)
}

fn diff_noise(line: &str) -> bool {
    line.is_empty()
        || line.starts_with('\\')
        || line.starts_with("diff ")
        || line.starts_with("index ")
}

/// Parse `@@ -a,b +c,d @@` → (new_start, new_count)
pub(crate) fn parse_hunk_new_ranges(diff: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for line in diff.lines() {
        if !line.starts_with("@@") {
            continue;
        }
        // @@ -10,5 +12,7 @@
        let Some(plus) = line.split('+').nth(1) else {
            continue;
        };
        let coords = plus.split_whitespace().next().unwrap_or("");
        let mut parts = coords.split(',');
        let start: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        let count: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        if start > 0 {
            out.push((start, count.max(1)));
        }
    }
    out
}

/// One `@@`-delimited hunk: its full text (header line included) and the
/// new-file line range it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffHunk {
    pub text: String,
    pub new_start: usize,
    pub new_end: usize,
}

/// Split a unified diff into preamble (file headers before the first `@@`)
/// and hunks. Non-`@@` lines after a hunk started stay with that hunk.
pub(crate) fn split_diff_hunks(diff: &str) -> (String, Vec<DiffHunk>) {
    let mut preamble = String::new();
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut in_hunk = false;
    let mut cur: Vec<&str> = Vec::new();
    let mut cur_range = (1usize, 1usize);

    let flush = |cur: &mut Vec<&str>, cur_range: (usize, usize), hunks: &mut Vec<DiffHunk>| {
        if cur.is_empty() {
            return;
        }
        let text = cur.join("\n");
        cur.clear();
        hunks.push(DiffHunk {
            text: format!("{text}\n"),
            new_start: cur_range.0,
            new_end: cur_range.1,
        });
    };

    for line in diff.lines() {
        if line.starts_with("@@") {
            flush(&mut cur, cur_range, &mut hunks);
            in_hunk = true;
            cur_range = parse_hunk_new_range(line);
            cur.push(line);
            continue;
        }
        if in_hunk {
            cur.push(line);
        } else {
            preamble.push_str(line);
            preamble.push('\n');
        }
    }
    flush(&mut cur, cur_range, &mut hunks);
    (preamble, hunks)
}

/// `@@ -a,b +c,d @@` → (c, c + d - 1) inclusive on the new side.
fn parse_hunk_new_range(header: &str) -> (usize, usize) {
    let Some(plus) = header.split('+').nth(1) else {
        return (1, 1);
    };
    let coords = plus.split_whitespace().next().unwrap_or("");
    let mut parts = coords.split(',');
    let start: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let count: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    if start == 0 {
        return (1, 1);
    }
    (start, start + count.saturating_sub(1).max(0))
}

/// Cut a unified diff to `budget` chars at hunk boundaries — never mid-hunk.
/// Returns the kept text and the hunks that did not fit (in order), so a
/// later pass can re-append them whole.
pub(crate) fn take_diff_budget(diff: &str, budget: usize) -> (String, Vec<DiffHunk>) {
    if diff.chars().count() <= budget {
        return (diff.to_string(), Vec::new());
    }
    let (preamble, hunks) = split_diff_hunks(diff);
    let preamble_len = preamble.chars().count();
    if preamble_len >= budget {
        // No room for even one hunk — keep nothing; report all hunks dropped.
        return (String::new(), hunks);
    }
    let mut kept = preamble;
    let mut used = preamble_len;
    let mut idx = 0;
    while idx < hunks.len() {
        let len = hunks[idx].text.chars().count();
        if used + len > budget {
            break;
        }
        kept.push_str(&hunks[idx].text);
        used += len;
        idx += 1;
    }
    if kept.chars().count() > preamble_len && !kept.ends_with('\n') {
        kept.push('\n');
    }
    (kept, hunks.into_iter().skip(idx).collect())
}

pub(crate) fn merge_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_by_key(|r| r.0);
    let mut out = vec![ranges[0]];
    for (lo, hi) in ranges.into_iter().skip(1) {
        let last = out.last_mut().unwrap();
        if lo <= last.1.saturating_add(2) {
            last.1 = last.1.max(hi);
        } else {
            out.push((lo, hi));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_ranges_skip_context_and_deletions() {
        let diff = "@@ -1,4 +1,4 @@\n keep\n-old\n+new\n keep\n+also\n";
        assert_eq!(added_line_ranges(diff), vec![(2, 2), (4, 4)]);
    }

    #[test]
    fn parse_hunk_ranges() {
        let diff = "@@ -1,2 +10,3 @@\n+a\n@@ -5 +20 @@\n+b\n";
        let r = parse_hunk_new_ranges(diff);
        assert_eq!(r, vec![(10, 3), (20, 1)]);
    }

    const SAMPLE: &str = "\
diff --git a/x.rs b/x.rs
index 111..222 100644
--- a/x.rs
+++ b/x.rs
@@ -1,2 +1,2 @@
 context
-old
+new
@@ -8,3 +8,4 @@
 ctx
+added
 more
";

    #[test]
    fn split_keeps_preamble_and_hunks() {
        let (preamble, hunks) = split_diff_hunks(SAMPLE);
        assert!(preamble.contains("diff --git"));
        assert_eq!(hunks.len(), 2);
        assert_eq!((hunks[0].new_start, hunks[0].new_end), (1, 2));
        assert_eq!((hunks[1].new_start, hunks[1].new_end), (8, 11));
    }

    #[test]
    fn diff_cut_never_mid_hunk() {
        // Budget fits preamble + first hunk only.
        let (kept, dropped) = take_diff_budget(SAMPLE, 120);
        assert!(kept.contains("@@ -1,2 +1,2 @@"));
        assert!(!kept.contains("@@ -8,3 +8,4 @@"));
        assert_eq!(dropped.len(), 1);
        assert_eq!((dropped[0].new_start, dropped[0].new_end), (8, 11));
        // Kept text always ends on a hunk boundary — no partial last line.
        assert!(kept.ends_with("+new\n"));
    }

    #[test]
    fn diff_small_enough_untouched() {
        let (kept, dropped) = take_diff_budget(SAMPLE, 10_000);
        assert_eq!(kept, SAMPLE);
        assert!(dropped.is_empty());
    }

    #[test]
    fn diff_zero_budget_drops_all_hunks_in_order() {
        let (_, dropped) = take_diff_budget(SAMPLE, 1);
        let ranges: Vec<_> = dropped.iter().map(|h| (h.new_start, h.new_end)).collect();
        assert_eq!(ranges, vec![(1, 2), (8, 11)]);
    }

    #[test]
    fn dropped_hunks_re_appendable_whole() {
        let (_, dropped) = take_diff_budget(SAMPLE, 1);
        assert_eq!(dropped.len(), 2);
        assert!(dropped[0].text.starts_with("@@ -1,2 +1,2 @@"));
        assert!(dropped[0].text.ends_with("+new\n"));
    }

    #[test]
    fn merge_adjacent() {
        let m = merge_ranges(vec![(1, 5), (6, 8), (20, 22)]);
        assert_eq!(m, vec![(1, 8), (20, 22)]);
    }
}
