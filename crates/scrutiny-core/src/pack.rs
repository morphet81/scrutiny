use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{ensure_config, find_shipped_default, load_config, PackConfig};
use crate::git;
use crate::map::MapReport;
use crate::paths::{temp_artifact_path, write_json_pretty};
use crate::score::Tier;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackReport {
    pub version: u32,
    pub map_path: String,
    pub repo: String,
    pub branch: String,
    pub base: String,
    pub head: String,
    pub tier: Tier,
    pub max_chars: usize,
    pub chars_used: usize,
    pub truncated: bool,
    pub architecture_risk: bool,
    pub needs_full_file: Vec<String>,
    pub slices: Vec<PackSlice>,
    pub doc_digests: Vec<DocDigest>,
    /// Optional markdown companion path (same stem .md) when written.
    pub markdown_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackSlice {
    pub path: String,
    pub kind: String,
    pub unified_diff: String,
    pub symbol_slices: Vec<SymbolSlice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSlice {
    pub label: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocDigest {
    pub path: String,
    pub headings: Vec<String>,
    pub preview: String,
}

pub fn run_pack(map_path: &Path, cwd: &Path) -> Result<(PackReport, PathBuf)> {
    let text = fs::read_to_string(map_path)
        .with_context(|| format!("read map {}", map_path.display()))?;
    let map: MapReport = serde_json::from_str(&text).context("parse map json")?;

    let shipped = find_shipped_default(&std::env::current_exe().unwrap_or_else(|_| cwd.to_path_buf()));
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;
    let pack_cfg = &cfg.pack;

    let repo = git::discover_repo(cwd)?;
    let mut budget = pack_cfg.max_chars;
    let mut truncated = false;
    let mut chars_used = 0usize;
    let mut slices = Vec::new();
    let mut needs_full_file = Vec::new();

    let mut paths: Vec<(String, String)> = Vec::new();
    for s in &map.source_to_review {
        paths.push((s.path.clone(), "source".into()));
    }
    for t in &map.tests_related {
        paths.push((t.path.clone(), "test".into()));
    }

    for (path, kind) in paths {
        if budget == 0 {
            truncated = true;
            break;
        }
        let diff = git::diff_unified_paths(&repo.root, &map.base, &map.head, &[path.clone()])
            .unwrap_or_default();
        if diff.is_empty() {
            continue;
        }

        let mut remaining = budget;
        let (diff_part, diff_trunc) = take_budget(&diff, remaining);
        remaining = remaining.saturating_sub(diff_part.len());
        if diff_trunc {
            truncated = true;
            needs_full_file.push(path.clone());
        }

        let symbols = if remaining > 0 {
            let (syms, sym_trunc) =
                build_symbol_slices(&repo.root, &map.head, &path, &diff, pack_cfg, remaining)?;
            if sym_trunc {
                truncated = true;
            }
            let used: usize = syms.iter().map(|s| s.content.len()).sum();
            remaining = remaining.saturating_sub(used);
            syms
        } else {
            Vec::new()
        };

        let slice_chars = diff_part.len()
            + symbols.iter().map(|s| s.content.len()).sum::<usize>();
        chars_used += slice_chars;
        budget = remaining;

        slices.push(PackSlice {
            path,
            kind,
            unified_diff: diff_part,
            symbol_slices: symbols,
        });
    }

    let mut doc_digests = Vec::new();
    for d in &map.docs_semantic {
        if budget == 0 {
            truncated = true;
            break;
        }
        let digest = build_doc_digest(&repo.root, &map.head, &d.path, pack_cfg)?;
        let cost = digest.preview.len() + digest.headings.iter().map(|h| h.len()).sum::<usize>();
        if cost > budget {
            truncated = true;
            let (preview, _) = take_budget(&digest.preview, budget);
            chars_used += preview.len();
            doc_digests.push(DocDigest {
                path: digest.path,
                headings: digest.headings,
                preview,
            });
            break;
        }
        chars_used += cost;
        budget = budget.saturating_sub(cost);
        doc_digests.push(digest);
    }

    let architecture_risk = map
        .risk_tags
        .iter()
        .any(|t| t.tag == "security")
        || matches!(map.tier, Tier::L | Tier::Xl)
        || map.source_to_review.len() >= 12;

    let report = PackReport {
        version: 1,
        map_path: map_path.display().to_string(),
        repo: map.repo.clone(),
        branch: map.branch.clone(),
        base: map.base.clone(),
        head: map.head.clone(),
        tier: map.tier,
        max_chars: pack_cfg.max_chars,
        chars_used,
        truncated,
        architecture_risk,
        needs_full_file,
        slices,
        doc_digests,
        markdown_path: None,
    };

    let out = temp_artifact_path(&map.repo, &map.branch, "pack");
    write_json_pretty(&out, &report)?;

    // Optional markdown companion for human/agent skim
    let md_path = out.with_extension("md");
    let md = render_pack_markdown(&report);
    fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;

    let mut report = report;
    report.markdown_path = Some(md_path.display().to_string());
    write_json_pretty(&out, &report)?;

    Ok((report, out))
}

fn take_budget(s: &str, budget: usize) -> (String, bool) {
    if s.len() <= budget {
        return (s.to_string(), false);
    }
    let mut end = budget.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}\n…[truncated]", &s[..end]), true)
}

fn build_symbol_slices(
    root: &Path,
    head: &str,
    path: &str,
    diff: &str,
    cfg: &PackConfig,
    budget: usize,
) -> Result<(Vec<SymbolSlice>, bool)> {
    let file_text = show_file(root, head, path).unwrap_or_default();
    if file_text.is_empty() {
        return Ok((Vec::new(), false));
    }
    let lines: Vec<&str> = file_text.lines().collect();
    let hunk_lines = parse_hunk_new_ranges(diff);
    if hunk_lines.is_empty() {
        return Ok((Vec::new(), false));
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (start, count) in hunk_lines {
        let end = start.saturating_add(count.saturating_sub(1)).max(start);
        let (lo, hi) = expand_brace_range(&lines, start, end, cfg.symbol_context_lines);
        ranges.push((lo, hi));
    }
    ranges = merge_ranges(ranges);

    let mut out = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for (lo, hi) in ranges {
        if used >= budget {
            truncated = true;
            break;
        }
        let content: String = lines
            .get(lo.saturating_sub(1)..hi.min(lines.len()))
            .map(|chunk| chunk.join("\n"))
            .unwrap_or_default();
        let (content, trunc) = take_budget(&content, budget.saturating_sub(used));
        if trunc {
            truncated = true;
        }
        used += content.len();
        out.push(SymbolSlice {
            label: format!("{path}:{lo}-{hi}"),
            start_line: lo,
            end_line: hi,
            content,
        });
    }
    Ok((out, truncated))
}

fn show_file(root: &Path, head: &str, path: &str) -> Result<String> {
    let spec = format!("{head}:{path}");
    git::git_stdout(root, &["show", &spec])
}

/// Parse `@@ -a,b +c,d @@` → (new_start, new_count)
fn parse_hunk_new_ranges(diff: &str) -> Vec<(usize, usize)> {
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

fn expand_brace_range(
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

fn looks_like_decl(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("fn ")
        || t.starts_with("pub fn ")
        || t.starts_with("function ")
        || t.starts_with("export function ")
        || t.starts_with("export const ")
        || t.starts_with("class ")
        || t.starts_with("pub struct ")
        || t.starts_with("impl ")
        || t.contains("=> {")
        || t.ends_with('{')
}

fn merge_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
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

fn build_doc_digest(root: &Path, head: &str, path: &str, cfg: &PackConfig) -> Result<DocDigest> {
    let text = show_file(root, head, path).unwrap_or_default();
    let mut headings = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            headings.push(t.to_string());
            if headings.len() >= 30 {
                break;
            }
        }
    }
    let preview: String = text
        .lines()
        .take(cfg.doc_digest_lines)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(DocDigest {
        path: path.to_string(),
        headings,
        preview,
    })
}

fn render_pack_markdown(pack: &PackReport) -> String {
    let mut md = String::new();
    md.push_str("# Scrutiny pack\n\n");
    md.push_str(&format!(
        "tier={} chars={}/{} truncated={} architecture_risk={}\n\n",
        pack.tier, pack.chars_used, pack.max_chars, pack.truncated, pack.architecture_risk
    ));
    for s in &pack.slices {
        md.push_str(&format!("## {} ({})\n\n```diff\n{}\n```\n\n", s.path, s.kind, s.unified_diff));
        for sym in &s.symbol_slices {
            md.push_str(&format!(
                "### {}\n\n```\n{}\n```\n\n",
                sym.label, sym.content
            ));
        }
    }
    for d in &pack.doc_digests {
        md.push_str(&format!("## doc {}\n\n", d.path));
        if !d.headings.is_empty() {
            md.push_str("Headings:\n");
            for h in &d.headings {
                md.push_str(&format!("- {h}\n"));
            }
            md.push('\n');
        }
        md.push_str(&format!("```\n{}\n```\n\n", d.preview));
    }
    if !pack.needs_full_file.is_empty() {
        md.push_str("## needs_full_file\n\n");
        for p in &pack.needs_full_file {
            md.push_str(&format!("- {p}\n"));
        }
    }
    md
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

    #[test]
    fn budget_truncates() {
        let (s, t) = take_budget("abcdef", 3);
        assert!(t);
        assert!(s.contains("truncated"));
    }
}
