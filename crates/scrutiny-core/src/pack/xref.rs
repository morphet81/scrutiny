//! Cross-file signature resolution: for identifiers the diff references but does not
//! define, locate their definition elsewhere in the repo and emit a one-line signature.
//! Bodies are never included. Bounded and deterministic (no network).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use crate::config::PackConfig;
use crate::git;
use crate::map::MapReport;
use crate::treesitter::{self, lang_for_path};

use super::{show_file, ReferencedSignature};

pub(crate) struct RefUse {
    pub name: String,
    pub from: String,
}

/// Common builtins / keywords that are never worth resolving cross-file.
fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "puts" | "print" | "require" | "require_relative" | "new" | "raise" | "loop" | "lambda"
            | "println" | "print!" | "format" | "vec" | "Some" | "None" | "Ok" | "Err" | "String"
            | "Vec" | "Box" | "self" | "super" | "len" | "clone" | "to_string" | "into"
            | "unwrap" | "expect" | "iter" | "map" | "filter" | "collect" | "push"
            | "console" | "log" | "Error" | "Object" | "Array" | "Promise" | "JSON"
    )
}

pub(crate) fn resolve(
    root: &Path,
    map: &MapReport,
    referrers: &[RefUse],
    defined: &HashSet<String>,
    cfg: &PackConfig,
) -> (Vec<ReferencedSignature>, usize) {
    if !cfg.enable_xref {
        return (Vec::new(), 0);
    }

    let mut by_name: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for r in referrers {
        if defined.contains(&r.name) || is_builtin(&r.name) {
            continue;
        }
        by_name.entry(r.name.clone()).or_default().insert(r.from.clone());
    }
    let mut names: Vec<String> = by_name.keys().cloned().collect();
    names.truncate(cfg.xref_max_symbols);
    if names.is_empty() {
        return (Vec::new(), 0);
    }
    let wanted: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();

    // Candidate definition files: tracked, grammar-backed, not part of the change.
    let changed: HashSet<String> = map
        .source_to_review
        .iter()
        .map(|f| f.path.clone())
        .chain(map.tests_related.iter().map(|f| f.path.clone()))
        .collect();
    let listing = git::git_stdout(root, &["ls-files"]).unwrap_or_default();
    let mut files: Vec<String> = listing
        .lines()
        .map(|s| s.to_string())
        .filter(|f| lang_for_path(f).is_some() && !changed.contains(f))
        .collect();
    files.sort();
    files.truncate(cfg.xref_max_files_scanned);

    // Scan each candidate once; first file (sorted) defining a wanted name wins.
    let mut found: BTreeMap<String, (String, usize, String)> = BTreeMap::new();
    for f in &files {
        if found.len() == names.len() {
            break;
        }
        let src = show_file(root, &map.head, f).unwrap_or_default();
        if src.is_empty() {
            continue;
        }
        let Some(decls) = treesitter::outline(f, &src) else {
            continue;
        };
        for d in decls {
            if wanted.contains(d.name.as_str()) && !found.contains_key(&d.name) {
                found.insert(d.name.clone(), (f.clone(), d.start, d.signature));
            }
        }
    }

    let mut sigs = Vec::new();
    let mut chars = 0usize;
    for name in &names {
        let Some((def_path, def_line, signature)) = found.get(name) else {
            continue;
        };
        let referenced_from: Vec<String> = by_name[name].iter().cloned().collect();
        let cost = signature.len() + name.len() + def_path.len() + 24;
        if chars + cost > cfg.xref_char_budget {
            break;
        }
        chars += cost;
        sigs.push(ReferencedSignature {
            name: name.clone(),
            def_path: def_path.clone(),
            def_line: *def_line,
            signature: signature.clone(),
            referenced_from,
        });
    }

    (sigs, chars)
}
