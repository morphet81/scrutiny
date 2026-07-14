//! Change-scoped locale key parity (JSON flat/nested).

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::ScanI18nConfig;
use crate::map::MapReport;
use crate::pack::show_file;
use crate::scan::Finding;

pub fn collect_i18n_findings(
    root: &Path,
    map: &MapReport,
    cfg: &ScanI18nConfig,
) -> Result<Vec<Finding>> {
    if !cfg.enable {
        return Ok(Vec::new());
    }
    let globset = build_globs(&cfg.path_globs)?;
    let mut locale_files: Vec<String> = map
        .noise_skipped
        .iter()
        .filter(|n| n.reason == "i18n_deterministic")
        .map(|n| n.path.clone())
        .collect();

    // Also pick from eval-mapped paths that somehow appear elsewhere
    for s in &map.source_to_review {
        if globset.is_match(&s.path) {
            locale_files.push(s.path.clone());
        }
    }

    // From docs? no. Discover via noise + any eval files via map noise only is incomplete —
    // map should list i18n in noise_skipped. Also scan changed paths from git via glob on all map noise +
    // re-read: collect from all noise and we need i18n paths passed in. Call sites push them.

    locale_files.sort();
    locale_files.dedup();
    if locale_files.is_empty() {
        return Ok(Vec::new());
    }

    // Group by directory; stem = locale name
    let mut by_dir: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for path in &locale_files {
        let p = Path::new(path);
        let locale = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let dir = p
            .parent()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default();
        if locale.is_empty() {
            continue;
        }
        by_dir.entry(dir).or_default().push((locale, path.clone()));
    }

    let mut findings = Vec::new();
    for (_dir, locales) in &by_dir {
        let Some((_, ref_path)) = locales
            .iter()
            .find(|(loc, _)| loc.eq_ignore_ascii_case(&cfg.reference_locale))
        else {
            // No reference locale in this change set — compare against first as weak fallback only when ≥2
            if locales.len() < 2 {
                continue;
            }
            findings.extend(parity_across_changed(
                root,
                map,
                cfg,
                &locales[0].1,
                &locales[0].0,
                locales,
            )?);
            continue;
        };
        findings.extend(parity_across_changed(
            root,
            map,
            cfg,
            ref_path,
            &cfg.reference_locale,
            locales,
        )?);
    }
    Ok(findings)
}

fn build_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).with_context(|| format!("bad i18n glob: {p}"))?);
    }
    Ok(b.build()?)
}

fn parity_across_changed(
    root: &Path,
    map: &MapReport,
    cfg: &ScanI18nConfig,
    ref_path: &str,
    ref_locale: &str,
    locales: &[(String, String)],
) -> Result<Vec<Finding>> {
    let head_ref = show_file(root, &map.head, ref_path).unwrap_or_default();
    let base_ref = show_file(root, &map.base, ref_path).unwrap_or_default();
    let head_keys = flatten_json(&head_ref);
    let base_keys = flatten_json(&base_ref);

    // Keys added or value-changed in reference
    let mut touched: BTreeSet<String> = BTreeSet::new();
    for (k, v) in &head_keys {
        match base_keys.get(k) {
            None => {
                touched.insert(k.clone());
            }
            Some(old) if old != v => {
                touched.insert(k.clone());
            }
            _ => {}
        }
    }
    if cfg.full_catalog {
        touched.extend(head_keys.keys().cloned());
    }
    if touched.is_empty() {
        // Also: keys added only in a non-reference locale this PR
        for (loc, path) in locales {
            if loc.eq_ignore_ascii_case(ref_locale) {
                continue;
            }
            let head = show_file(root, &map.head, path).unwrap_or_default();
            let base = show_file(root, &map.base, path).unwrap_or_default();
            let hk = flatten_json(&head);
            let bk = flatten_json(&base);
            for (k, _) in &hk {
                if !bk.contains_key(k) {
                    touched.insert(k.clone());
                }
            }
        }
    }
    if touched.is_empty() {
        return Ok(Vec::new());
    }

    let mut findings = Vec::new();
    for (loc, path) in locales {
        if loc.eq_ignore_ascii_case(ref_locale) {
            continue;
        }
        let head = show_file(root, &map.head, path).unwrap_or_default();
        let keys = flatten_json(&head);
        let mut missing = Vec::new();
        let mut empty = Vec::new();
        let mut placeholder = Vec::new();
        for k in &touched {
            match keys.get(k) {
                None => missing.push(k.clone()),
                Some(v) if cfg.check_empty_values && v.trim().is_empty() => empty.push(k.clone()),
                Some(v) if cfg.check_placeholders => {
                    if let Some(ref_v) = head_keys.get(k) {
                        let a = placeholders(ref_v);
                        let b = placeholders(v);
                        if a != b {
                            placeholder.push(k.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        if !missing.is_empty() {
            findings.push(finding(
                "i18n key missing in locale",
                format!(
                    "Locale `{loc}` missing {} key(s) present/changed in `{ref_locale}`: {}",
                    missing.len(),
                    missing.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
                ),
                &format!("Add the missing key(s) to `{path}`."),
                "warning",
                "scan.i18n_parity",
                vec![path.clone(), ref_path.to_string()],
                "i18n",
            ));
        }
        if !empty.is_empty() {
            findings.push(finding(
                "i18n empty translation",
                format!(
                    "Locale `{loc}` has empty value for: {}",
                    empty.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
                ),
                &format!("Fill translations in `{path}`."),
                "warning",
                "scan.i18n_parity",
                vec![path.clone()],
                "i18n",
            ));
        }
        if !placeholder.is_empty() {
            findings.push(finding(
                "i18n placeholder mismatch",
                format!(
                    "Locale `{loc}` placeholder set differs from `{ref_locale}` for: {}",
                    placeholder
                        .iter()
                        .take(8)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "Align `{name}` / `{{name}}` placeholders with the reference locale.",
                "warning",
                "scan.i18n_parity",
                vec![path.clone(), ref_path.to_string()],
                "i18n",
            ));
        }
    }
    Ok(findings)
}

fn finding(
    title: &str,
    explanation: String,
    proposed_fix: &str,
    severity: &str,
    source: &str,
    paths: Vec<String>,
    bucket: &str,
) -> Finding {
    Finding {
        number: 0,
        title: title.into(),
        explanation,
        proposed_fix: proposed_fix.into(),
        fix_options: Vec::new(),
        severity: crate::scan::normalize_severity(severity),
        source: source.into(),
        paths,
        bucket: bucket.into(),
        line: None,
        start_line: None,
    }
}

fn flatten_json(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return out;
    };
    flatten_value("", &v, &mut out);
    out
}

fn flatten_value(prefix: &str, v: &Value, out: &mut BTreeMap<String, String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_value(&key, child, out);
            }
        }
        Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        Value::Number(n) => {
            out.insert(prefix.to_string(), n.to_string());
        }
        Value::Bool(b) => {
            out.insert(prefix.to_string(), b.to_string());
        }
        Value::Null => {
            out.insert(prefix.to_string(), String::new());
        }
        Value::Array(arr) => {
            for (i, child) in arr.iter().enumerate() {
                flatten_value(&format!("{prefix}[{i}]"), child, out);
            }
        }
    }
}

fn placeholders(s: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    // {{name}} and {name}
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let double = i + 1 < bytes.len() && bytes[i + 1] == b'{';
            let start = if double { i + 2 } else { i + 1 };
            if let Some(rel) = s[start..].find('}') {
                let end = start + rel;
                let name = s[start..end].trim();
                if !name.is_empty() && !name.contains('{') {
                    set.insert(name.to_string());
                }
                i = end + 1;
                if double && i < bytes.len() && bytes[i] == b'}' {
                    i += 1;
                }
                continue;
            }
        }
        i += 1;
    }
    set
}

/// Paths from eval that match i18n globs (for map bucket).
pub fn is_i18n_path(path: &str, cfg: &ScanI18nConfig) -> bool {
    let Ok(gs) = build_globs(&cfg.path_globs) else {
        return crate::taxonomy::is_i18n(path);
    };
    gs.is_match(path) || crate::taxonomy::is_i18n(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_nested() {
        let m = flatten_json(r#"{"a":{"b":"x"},"c":"y"}"#);
        assert_eq!(m.get("a.b").map(String::as_str), Some("x"));
        assert_eq!(m.get("c").map(String::as_str), Some("y"));
    }

    #[test]
    fn placeholder_sets() {
        let a = placeholders("Hello {{name}} {count}");
        assert!(a.contains("name"));
        assert!(a.contains("count"));
    }
}
