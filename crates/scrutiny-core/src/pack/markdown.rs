use super::PackReport;

pub(crate) fn render_pack_markdown(pack: &PackReport) -> String {
    let mut md = String::new();
    md.push_str("# Scrutiny pack\n\n");
    md.push_str(&format!(
        "tier={} chars={}/{} truncated={} architecture_risk={}\n\n",
        pack.tier, pack.chars_used, pack.max_chars, pack.truncated, pack.architecture_risk
    ));

    md.push_str("## fetch\n\n");
    md.push_str(&format!("{}\n\n", pack.fetch.note));
    md.push_str(&format!(
        "base={} head={} repo_root={}\ntemplate: `{}`\n\n",
        pack.fetch.base, pack.fetch.head, pack.fetch.repo_root, pack.fetch.per_file_cmd_template
    ));

    for m in &pack.manifest {
        md.push_str(&format!(
            "## {} ({}){}\n\n",
            m.path,
            m.kind,
            if m.full_file_omitted {
                " — body partially omitted"
            } else {
                ""
            }
        ));
        if !m.outline.is_empty() {
            md.push_str("Outline:\n");
            for e in &m.outline {
                md.push_str(&format!(
                    "- {} {} ({}-{}){}\n",
                    e.kind,
                    e.name,
                    e.start_line,
                    e.end_line,
                    if e.in_diff { " *" } else { "" }
                ));
            }
            md.push('\n');
        }
        if let Some(s) = pack.slices.iter().find(|s| s.path == m.path) {
            md.push_str(&format!("```diff\n{}\n```\n\n", s.unified_diff));
            for sym in &s.symbol_slices {
                md.push_str(&format!("### {}\n\n```\n{}\n```\n\n", sym.label, sym.content));
            }
        }
        if !m.dropped_regions.is_empty() {
            md.push_str("Dropped (fetch to read):\n");
            for d in &m.dropped_regions {
                md.push_str(&format!("- {} → `{}`\n", d.label, d.fetch_cmd));
            }
            md.push('\n');
        }
    }

    if !pack.referenced_signatures.is_empty() {
        md.push_str("## referenced signatures\n\n");
        md.push_str("Symbols the diff calls but does not change (read the def for the body):\n\n");
        for r in &pack.referenced_signatures {
            md.push_str(&format!(
                "- `{}` — {} `{}:{}`\n",
                r.signature, r.name, r.def_path, r.def_line
            ));
        }
        md.push('\n');
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
