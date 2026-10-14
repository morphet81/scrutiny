use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{ensure_config, find_shipped_default, load_config};
use crate::git;
use crate::map::MapReport;
use crate::paths::{temp_artifact_path, write_json_pretty};
use crate::score::Tier;

mod budget;
mod doc;
mod hunk;
mod markdown;
mod outline;
mod slice;
mod xref;

use budget::{allocate, FileWork};
use doc::build_doc_digest;
use markdown::render_pack_markdown;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackReport {
    pub version: u32,
    pub map_path: String,
    pub repo: String,
    #[serde(default)]
    pub repo_root: String,
    pub branch: String,
    pub base: String,
    pub head: String,
    pub tier: Tier,
    pub max_chars: usize,
    pub chars_used: usize,
    pub truncated: bool,
    pub architecture_risk: bool,
    /// How to reach any source the pack omitted. Agents run in the repo cwd.
    #[serde(default)]
    pub fetch: FetchContract,
    /// Shape of every changed file (outline is always present, even if body dropped).
    #[serde(default)]
    pub manifest: Vec<FileManifest>,
    /// Signatures of symbols the diff references but does not define. Bodies not included.
    #[serde(default)]
    pub referenced_signatures: Vec<ReferencedSignature>,
    pub needs_full_file: Vec<String>,
    pub slices: Vec<PackSlice>,
    pub doc_digests: Vec<DocDigest>,
    /// Optional markdown companion path (same stem .md) when written.
    pub markdown_path: Option<String>,
    #[serde(default)]
    pub explore: ExploreContract,
    /// Top dropped regions inlined verbatim (within `annex_char_budget`).
    #[serde(default)]
    pub annex: Vec<AnnexRegion>,
    /// Chars of diff hunks + symbol regions that did not fit `max_chars`.
    #[serde(default)]
    pub dropped_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnexRegion {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExploreContract {
    pub enable: bool,
    pub max_extra_reads: u32,
    pub max_extra_chars: usize,
    pub prefer_read_over_bash: bool,
    pub allow_repo_grep: bool,
    pub require_pack_path_hint: bool,
    pub allowed_paths: Vec<String>,
    pub note: String,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FetchContract {
    pub base: String,
    pub head: String,
    pub repo_root: String,
    pub note: String,
    pub per_file_cmd_template: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileManifest {
    pub path: String,
    pub kind: String,
    pub head_show_cmd: String,
    pub outline: Vec<OutlineEntry>,
    pub included_symbols: Vec<String>,
    pub dropped_regions: Vec<DroppedRegion>,
    pub full_file_omitted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutlineEntry {
    pub kind: String,
    pub name: String,
    pub signature: String,
    pub start_line: usize,
    pub end_line: usize,
    pub in_diff: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DroppedRegion {
    pub label: String,
    pub start_line: usize,
    pub end_line: usize,
    pub fetch_cmd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencedSignature {
    pub name: String,
    pub def_path: String,
    pub def_line: usize,
    pub signature: String,
    pub referenced_from: Vec<String>,
}

pub fn run_pack(map_path: &Path, cwd: &Path) -> Result<(PackReport, PathBuf)> {
    run_pack_filtered(map_path, cwd, None, "pack")
}

/// Per-reviewer-bucket pack: only `paths` land in slices, but the shard gets
/// the full `max_chars` budget. Written as `pack-shard-<label>.json`.
pub fn run_pack_shard(
    map_path: &Path,
    cwd: &Path,
    paths: &[String],
    label: &str,
) -> Result<(PackReport, PathBuf)> {
    run_pack_filtered(map_path, cwd, Some(paths), &format!("pack-shard-{label}"))
}

fn run_pack_filtered(
    map_path: &Path,
    cwd: &Path,
    path_filter: Option<&[String]>,
    artifact_kind: &str,
) -> Result<(PackReport, PathBuf)> {
    let text =
        fs::read_to_string(map_path).with_context(|| format!("read map {}", map_path.display()))?;
    let map: MapReport = serde_json::from_str(&text).context("parse map json")?;

    let shipped =
        find_shipped_default(&std::env::current_exe().unwrap_or_else(|_| cwd.to_path_buf()));
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config(&cfg_path)?;
    let pack_cfg = &cfg.probe.pack;

    let repo = git::discover_repo(cwd)?;
    let repo_root = repo.root.display().to_string();

    let fetch = FetchContract {
        base: map.base.clone(),
        head: map.head.clone(),
        repo_root: repo_root.clone(),
        note: "Graduated exploration: prefer pack contents. Tier-1: only allowlisted \
               dropped_regions.fetch_cmd / explore.allowed_paths. Tier-2: ≤max_extra_reads \
               of pack-hinted paths. No whole-repo search."
            .into(),
        per_file_cmd_template: format!("git show {}:<path>", map.head),
    };

    // Build per-file work: diff + outline + candidate symbol regions.
    let filter = path_filter.map(|f| {
        f.iter()
            .map(|s| s.as_str())
            .collect::<std::collections::HashSet<_>>()
    });
    let mut paths: Vec<(String, String)> = Vec::new();
    for s in &map.source_to_review {
        if let Some(f) = &filter {
            if !f.contains(s.path.as_str()) {
                continue;
            }
        }
        paths.push((s.path.clone(), "source".into()));
    }
    for t in &map.tests_related {
        if let Some(f) = &filter {
            if !f.contains(t.path.as_str()) {
                continue;
            }
        }
        paths.push((t.path.clone(), "test".into()));
    }
    if paths.is_empty() {
        anyhow::bail!("pack: no changed files selected");
    }

    let mut work: Vec<FileWork> = Vec::new();
    let mut referrers: Vec<xref::RefUse> = Vec::new();
    let mut defined: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (path, kind) in paths {
        let diff = git::diff_unified_paths(
            &repo.root,
            &map.base,
            &map.head,
            std::slice::from_ref(&path),
        )
        .unwrap_or_default();
        if diff.is_empty() {
            continue;
        }
        let head_text = show_file(&repo.root, &map.head, &path).unwrap_or_default();
        let ranges = outline::changed_ranges(&diff);
        let file_outline = outline::build_outline(&path, &head_text, &ranges);
        let regions = outline::build_regions(&path, &head_text, &file_outline, &ranges, pack_cfg);

        for e in &file_outline {
            if !e.name.is_empty() {
                defined.insert(e.name.clone());
            }
        }
        if pack_cfg.enable_xref {
            if let Some(refs) = crate::treesitter::references(&path, &head_text, &ranges) {
                for r in refs {
                    referrers.push(xref::RefUse {
                        name: r.name,
                        from: path.clone(),
                    });
                }
            }
        }

        work.push(FileWork {
            path: path.clone(),
            kind,
            head_show_cmd: format!("git show {}:{}", map.head, path),
            diff,
            outline: file_outline,
            regions,
        });
    }

    // Cross-file referenced signatures (bounded; may be empty).
    let (referenced_signatures, xref_chars) =
        xref::resolve(&repo.root, &map, &referrers, &defined, pack_cfg);
    let reserve_xref = xref_chars.min(pack_cfg.max_chars / 4);

    let mut assembly = allocate(work, pack_cfg.max_chars, reserve_xref, pack_cfg);

    // Docs consume whatever remains under max_chars.
    let mut doc_digests = Vec::new();
    let mut budget = pack_cfg
        .max_chars
        .saturating_sub(assembly.chars_used + xref_chars);
    for d in &map.docs_semantic {
        if budget == 0 {
            assembly.truncated = true;
            break;
        }
        let digest = build_doc_digest(&repo.root, &map.head, &d.path, pack_cfg)?;
        let cost = digest.preview.len() + digest.headings.iter().map(|h| h.len()).sum::<usize>();
        if cost > budget {
            assembly.truncated = true;
            let (preview, _) = take_budget(&digest.preview, budget);
            assembly.chars_used += preview.len();
            doc_digests.push(DocDigest {
                path: digest.path,
                headings: digest.headings,
                preview,
            });
            break;
        }
        assembly.chars_used += cost;
        budget = budget.saturating_sub(cost);
        doc_digests.push(digest);
    }
    assembly.chars_used += xref_chars;

    let architecture_risk = map.risk_tags.iter().any(|t| t.tag == "security")
        || matches!(map.tier, Tier::L | Tier::Xl)
        || map.source_to_review.len() >= 12;

    let mut allowed_paths: Vec<String> = assembly.needs_full_file.clone();
    for m in &assembly.manifest {
        for d in &m.dropped_regions {
            // path is in label "path:start-end"
            if let Some(p) = d.label.split(':').next() {
                if !allowed_paths.iter().any(|x| x == p) {
                    allowed_paths.push(p.to_string());
                }
            }
        }
    }
    for r in &referenced_signatures {
        if !allowed_paths.iter().any(|x| x == &r.def_path) {
            allowed_paths.push(r.def_path.clone());
        }
    }

    let explore = ExploreContract {
        enable: pack_cfg.explore.enable,
        max_extra_reads: pack_cfg.explore.max_extra_reads,
        max_extra_chars: pack_cfg.explore.max_extra_chars,
        prefer_read_over_bash: pack_cfg.explore.prefer_read_over_bash,
        allow_repo_grep: pack_cfg.explore.allow_repo_grep,
        require_pack_path_hint: pack_cfg.explore.require_pack_path_hint,
        allowed_paths,
        note: "Tier0=pack; Tier1=allowed_paths/fetch_cmd; Tier2=≤max_extra_reads pack-hinted Reads"
            .into(),
    };

    let annex = build_annex(
        &repo.root,
        &map.head,
        &assembly.manifest,
        pack_cfg.annex_char_budget,
    );

    let report = PackReport {
        version: 2,
        map_path: map_path.display().to_string(),
        repo: map.repo.clone(),
        repo_root,
        branch: map.branch.clone(),
        base: map.base.clone(),
        head: map.head.clone(),
        tier: map.tier,
        max_chars: pack_cfg.max_chars,
        chars_used: assembly.chars_used,
        truncated: assembly.truncated,
        architecture_risk,
        fetch,
        manifest: assembly.manifest,
        referenced_signatures,
        needs_full_file: assembly.needs_full_file,
        slices: assembly.slices,
        doc_digests,
        markdown_path: None,
        explore,
        annex,
        dropped_chars: assembly.dropped_chars,
    };

    let out = temp_artifact_path(&map.repo, &map.branch, artifact_kind);
    write_json_pretty(&out, &report)?;

    let md_path = out.with_extension("md");
    let md = render_pack_markdown(&report);
    fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;

    let mut report = report;
    report.markdown_path = Some(md_path.display().to_string());
    write_json_pretty(&out, &report)?;

    Ok((report, out))
}

pub(crate) fn take_budget(s: &str, budget: usize) -> (String, bool) {
    if s.len() <= budget {
        return (s.to_string(), false);
    }
    const MARK: &str = "\n…[truncated]";
    if budget <= MARK.len() {
        return (String::new(), true);
    }
    let mut end = (budget - MARK.len()).min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}{MARK}", &s[..end]), true)
}

pub(crate) fn show_file(root: &Path, head: &str, path: &str) -> Result<String> {
    let spec = format!("{head}:{path}");
    git::git_stdout(root, &["show", &spec])
}

/// Inline the top dropped regions into the pack itself (within the annex
/// budget), so reviewers see the highest-value missing code without a fetch.
/// `dropped_regions` stays untouched as the fallback pointer list.
fn build_annex(
    root: &Path,
    head: &str,
    manifest: &[FileManifest],
    budget: usize,
) -> Vec<AnnexRegion> {
    let mut annex = Vec::new();
    if budget == 0 {
        return annex;
    }
    let mut used = 0usize;
    let mut seen = std::collections::HashSet::new();
    let mut cache: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in manifest {
        for d in &m.dropped_regions {
            if !seen.insert((m.path.clone(), d.start_line, d.end_line)) {
                continue;
            }
            let rem = budget.saturating_sub(used);
            if rem == 0 {
                return annex;
            }
            let text = cache
                .entry(m.path.clone())
                .or_insert_with(|| show_file(root, head, &m.path).unwrap_or_default());
            let content = slice_lines(text, d.start_line, d.end_line);
            if content.is_empty() {
                continue;
            }
            let (content, _) = take_budget(&content, rem);
            used += content.len();
            let partial = content.contains("…[truncated]");
            annex.push(AnnexRegion {
                path: m.path.clone(),
                start_line: d.start_line,
                end_line: d.end_line,
                content,
            });
            if partial {
                return annex;
            }
        }
    }
    annex
}

/// 1-based inclusive line slice.
fn slice_lines(text: &str, start: usize, end: usize) -> String {
    if end < start {
        return String::new();
    }
    text.lines()
        .skip(start.saturating_sub(1))
        .take(end - start + 1)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_truncates() {
        let long = "a".repeat(100);
        let (s, t) = take_budget(&long, 30);
        assert!(t);
        assert!(s.contains("truncated"));
        assert!(s.len() <= 30);
    }

    #[test]
    fn budget_no_marker_overrun() {
        // Total length never exceeds the budget, even accounting for the marker.
        let long = "x".repeat(1000);
        for b in [16, 20, 50, 200] {
            let (s, _) = take_budget(&long, b);
            assert!(s.len() <= b, "len {} > budget {b}", s.len());
        }
    }

    #[test]
    fn line_slice_bounds() {
        let text = "l1\nl2\nl3\nl4";
        assert_eq!(slice_lines(text, 2, 3), "l2\nl3");
        assert_eq!(slice_lines(text, 1, 1), "l1");
        assert_eq!(slice_lines(text, 3, 2), "");
        assert_eq!(slice_lines(text, 10, 20), "");
    }
}
