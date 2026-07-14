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
    fn parse_hunk_ranges() {
        let diff = "@@ -1,2 +10,3 @@\n+a\n@@ -5 +20 @@\n+b\n";
        let r = parse_hunk_new_ranges(diff);
        assert_eq!(r, vec![(10, 3), (20, 1)]);
    }

    #[test]
    fn merge_adjacent() {
        let m = merge_ranges(vec![(1, 5), (6, 8), (20, 22)]);
        assert_eq!(m, vec![(1, 8), (20, 22)]);
    }
}
