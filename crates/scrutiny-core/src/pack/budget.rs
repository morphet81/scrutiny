//! Fair budget allocator. Guarantees every changed file its outline + a capped diff +
//! its primary symbol before any file receives extra symbol bodies. Deterministic.

use crate::config::PackConfig;

use super::{take_budget, DroppedRegion, FileManifest, OutlineEntry, PackSlice, SymbolSlice};

pub(crate) struct Region {
    pub label: String,
    pub start: usize,
    pub end: usize,
    pub content: String,
}

pub(crate) struct FileWork {
    pub path: String,
    pub kind: String,
    pub head_show_cmd: String,
    pub diff: String,
    pub outline: Vec<OutlineEntry>,
    pub regions: Vec<Region>,
}

pub(crate) struct Assembly {
    pub slices: Vec<PackSlice>,
    pub manifest: Vec<FileManifest>,
    pub needs_full_file: Vec<String>,
    pub chars_used: usize,
    pub truncated: bool,
}

fn weight(kind: &str, cfg: &PackConfig) -> u32 {
    match kind {
        "source" => cfg.source_weight,
        "test" => cfg.test_weight,
        _ => cfg.doc_weight,
    }
}

fn outline_cost(o: &[OutlineEntry]) -> usize {
    // ~one rendered line per entry.
    o.iter()
        .map(|e| e.signature.len() + e.name.len() + e.kind.len() + 24)
        .sum()
}

struct Acc {
    diff: String,
    diff_trunc: bool,
    syms: Vec<SymbolSlice>,
    used: Vec<bool>,
    full_omitted: bool,
}

pub(crate) fn allocate(
    work: Vec<FileWork>,
    max_chars: usize,
    reserve_xref: usize,
    cfg: &PackConfig,
) -> Assembly {
    let pool = max_chars.saturating_sub(reserve_xref);

    // Priority order: heavier weight first; ties keep map order (stable sort over ascending indices).
    let mut order: Vec<usize> = (0..work.len()).collect();
    order.sort_by(|&a, &b| weight(&work[b].kind, cfg).cmp(&weight(&work[a].kind, cfg)));

    let mut accs: Vec<Acc> = work
        .iter()
        .map(|w| Acc {
            diff: String::new(),
            diff_trunc: false,
            syms: Vec::new(),
            used: vec![false; w.regions.len()],
            full_omitted: false,
        })
        .collect();

    let mut spent = 0usize;
    let mut truncated = false;

    // Pass A — mandatory reserve per file (outline + capped diff + primary region).
    for &i in &order {
        let w = &work[i];
        let o_cost = outline_cost(&w.outline);
        if spent + o_cost > pool {
            accs[i].full_omitted = true;
            truncated = true;
            continue;
        }
        let mut local = o_cost;

        let diff_cap = cfg.min_file_chars.min(pool.saturating_sub(spent + local));
        if diff_cap == 0 && !w.diff.is_empty() {
            accs[i].full_omitted = true;
            truncated = true;
        } else {
            let (dpart, dtrunc) = take_budget(&w.diff, diff_cap);
            if dtrunc {
                truncated = true;
                accs[i].diff_trunc = true;
            }
            local += dpart.len();
            accs[i].diff = dpart;
        }

        if let Some(r) = w.regions.first() {
            let rem = pool.saturating_sub(spent + local);
            if rem > 0 {
                let (c, ct) = take_budget(&r.content, rem);
                if ct {
                    truncated = true;
                }
                local += c.len();
                accs[i].syms.push(SymbolSlice {
                    label: r.label.clone(),
                    start_line: r.start,
                    end_line: r.end,
                    content: c,
                });
                accs[i].used[0] = true;
            } else {
                truncated = true;
            }
        }

        spent += local;
    }

    // Pass B — distribute remainder round-robin (one extra region per file per round, priority order).
    loop {
        let mut progressed = false;
        for &i in &order {
            let rem = pool.saturating_sub(spent);
            if rem == 0 {
                break;
            }
            let next = accs[i].used.iter().position(|u| !u);
            let Some(ri) = next else { continue };
            let r = &work[i].regions[ri];
            let (c, ct) = take_budget(&r.content, rem);
            if c.is_empty() {
                continue;
            }
            if ct {
                truncated = true;
            }
            spent += c.len();
            accs[i].syms.push(SymbolSlice {
                label: r.label.clone(),
                start_line: r.start,
                end_line: r.end,
                content: c,
            });
            accs[i].used[ri] = true;
            progressed = true;
        }
        if !progressed {
            break;
        }
    }

    // Assemble in original (map) order for stable output + partitioning.
    let mut slices = Vec::new();
    let mut manifest = Vec::new();
    let mut needs_full_file = Vec::new();

    for (i, w) in work.into_iter().enumerate() {
        let acc = std::mem::replace(
            &mut accs[i],
            Acc {
                diff: String::new(),
                diff_trunc: false,
                syms: Vec::new(),
                used: Vec::new(),
                full_omitted: false,
            },
        );

        let dropped_regions: Vec<DroppedRegion> = w
            .regions
            .iter()
            .enumerate()
            .filter(|(ri, _)| !acc.used.get(*ri).copied().unwrap_or(true))
            .map(|(_, r)| DroppedRegion {
                label: r.label.clone(),
                start_line: r.start,
                end_line: r.end,
                fetch_cmd: format!("{} | sed -n '{},{}p'", w.head_show_cmd, r.start, r.end),
            })
            .collect();

        let full_omitted = acc.full_omitted
            || (acc.diff_trunc && acc.syms.is_empty())
            || (!dropped_regions.is_empty()
                && acc
                    .syms
                    .first()
                    .map(|s| {
                        // Primary region heavily truncated or missing
                        let primary = w.regions.first().map(|r| r.content.len()).unwrap_or(0);
                        primary > 0 && s.content.len() * 2 < primary
                    })
                    .unwrap_or(true));
        if full_omitted {
            needs_full_file.push(w.path.clone());
        }

        let mut syms = acc.syms;
        syms.sort_by_key(|s| s.start_line);

        manifest.push(FileManifest {
            path: w.path.clone(),
            kind: w.kind.clone(),
            head_show_cmd: w.head_show_cmd.clone(),
            outline: w.outline,
            included_symbols: syms.iter().map(|s| s.label.clone()).collect(),
            dropped_regions,
            full_file_omitted: full_omitted,
        });

        slices.push(PackSlice {
            path: w.path,
            kind: w.kind,
            unified_diff: acc.diff,
            symbol_slices: syms,
        });
    }

    Assembly {
        slices,
        manifest,
        needs_full_file,
        chars_used: spent,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PackConfig {
        PackConfig::default()
    }

    fn work(path: &str, kind: &str, diff: &str, regions: Vec<(usize, usize, &str)>) -> FileWork {
        FileWork {
            path: path.into(),
            kind: kind.into(),
            head_show_cmd: format!("git show HEAD:{path}"),
            diff: diff.into(),
            outline: vec![OutlineEntry {
                kind: "fn".into(),
                name: "f".into(),
                signature: "fn f()".into(),
                start_line: 1,
                end_line: 3,
                in_diff: true,
            }],
            regions: regions
                .into_iter()
                .map(|(s, e, c)| Region {
                    label: format!("{path}:{s}-{e}"),
                    start: s,
                    end: e,
                    content: c.into(),
                })
                .collect(),
        }
    }

    #[test]
    fn every_file_gets_floor_when_starved() {
        // Tiny pool; many files. Each must still get outline + at least attempted diff.
        let w: Vec<FileWork> = (0..5)
            .map(|i| {
                work(
                    &format!("s{i}.rs"),
                    "source",
                    "@@ -1,1 +1,1 @@\n+let x = 1;\n",
                    vec![(1, 3, "fn f() { let x = 1; }")],
                )
            })
            .collect();
        let a = allocate(w, 400, 0, &cfg());
        assert_eq!(a.slices.len(), 5);
        // Every file carries its outline in the manifest even when starved.
        assert!(a.manifest.iter().all(|m| !m.outline.is_empty()));
        assert!(a.chars_used <= 400);
    }

    #[test]
    fn total_within_budget() {
        let big = "x".repeat(5000);
        let w = vec![
            work("a.rs", "source", "@@ -1 +1 @@\n+a\n", vec![(1, 200, &big)]),
            work("b.rs", "source", "@@ -1 +1 @@\n+b\n", vec![(1, 200, &big)]),
        ];
        let a = allocate(w, 3000, 0, &cfg());
        assert!(a.chars_used <= 3000);
        assert!(a.truncated);
    }

    #[test]
    fn source_served_before_test() {
        let big = "y".repeat(4000);
        let w = vec![
            work("t_spec.rb", "test", "@@ -1 +1 @@\n+t\n", vec![(1, 100, &big)]),
            work("src.rb", "source", "@@ -1 +1 @@\n+s\n", vec![(1, 100, &big)]),
        ];
        let a = allocate(w, 2500, 0, &cfg());
        let src = a.slices.iter().find(|s| s.path == "src.rb").unwrap();
        let test = a.slices.iter().find(|s| s.path == "t_spec.rb").unwrap();
        let src_body: usize = src.symbol_slices.iter().map(|s| s.content.len()).sum();
        let test_body: usize = test.symbol_slices.iter().map(|s| s.content.len()).sum();
        assert!(src_body >= test_body);
    }

    #[test]
    fn deterministic() {
        let mk = || {
            vec![
                work("a.rs", "source", "@@ -1 +1 @@\n+a\n", vec![(1, 5, "fn a() {}")]),
                work("b.rs", "source", "@@ -1 +1 @@\n+b\n", vec![(1, 5, "fn b() {}")]),
            ]
        };
        let a = allocate(mk(), 5000, 0, &cfg());
        let b = allocate(mk(), 5000, 0, &cfg());
        assert_eq!(a.chars_used, b.chars_used);
        assert_eq!(
            a.slices.iter().map(|s| s.path.clone()).collect::<Vec<_>>(),
            b.slices.iter().map(|s| s.path.clone()).collect::<Vec<_>>()
        );
    }
}
