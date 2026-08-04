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

        // Plural-aware filtering: remove unsupported plural-category keys
        let filtered_missing = if cfg.plural_aware_filtering {
            filter_unsupported_plural_keys(&missing, &touched, loc, cfg)
        } else {
            missing.clone()
        };

        if !filtered_missing.is_empty() {
            findings.push(finding(
                "i18n key missing in locale",
                format!(
                    "Locale `{loc}` missing {} key(s) present/changed in `{ref_locale}`: {}",
                    filtered_missing.len(),
                    filtered_missing.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
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

/// Filter missing keys to remove unsupported plural-category omissions.
/// Returns only keys that should warn: non-plural keys + plural keys the locale supports.
fn filter_unsupported_plural_keys(
    missing: &[String],
    all_ref_keys: &BTreeSet<String>,
    locale: &str,
    cfg: &ScanI18nConfig,
) -> Vec<String> {
    let supported = resolve_supported_plural_categories(locale, cfg);
    let plural_groups = identify_plural_groups(all_ref_keys);

    missing
        .iter()
        .filter(|k| {
            if let Some(suffix) = extract_plural_suffix(k) {
                // Plural key: keep if locale supports this category or locale unknown (conservative)
                if supported.is_empty() {
                    // Unknown locale → warn (conservative default)
                    true
                } else {
                    supported.contains(&suffix.to_string())
                }
            } else if is_part_of_plural_group(k, &plural_groups) {
                // Non-suffixed key in plural group → warn (base key matters)
                true
            } else {
                // Non-plural key → always warn
                true
            }
        })
        .cloned()
        .collect()
}

/// Extract plural category suffix from key (zero, one, two, few, many, other).
fn extract_plural_suffix(key: &str) -> Option<&str> {
    for suffix in ["_zero", "_one", "_two", "_few", "_many", "_other"] {
        if key.ends_with(suffix) {
            return Some(&suffix[1..]); // strip leading _
        }
    }
    None
}

/// Identify plural groups: sets of keys sharing a base with different plural suffixes.
fn identify_plural_groups(keys: &BTreeSet<String>) -> BTreeSet<String> {
    let mut groups = BTreeSet::new();
    for k in keys {
        if extract_plural_suffix(k).is_some() {
            // Extract base: "models.allocation_one" → "models.allocation"
            if let Some(pos) = k.rfind('_') {
                let base = &k[..pos];
                groups.insert(base.to_string());
            }
        }
    }
    groups
}

/// Check if a key is part of a plural group (its base appears in the group set).
fn is_part_of_plural_group(key: &str, groups: &BTreeSet<String>) -> bool {
    if let Some(pos) = key.rfind('_') {
        let base = &key[..pos];
        groups.contains(base)
    } else {
        false
    }
}

/// Resolve supported plural categories for a locale.
/// Returns empty vec for unknown locales (conservative: warn everything).
fn resolve_supported_plural_categories(
    locale: &str,
    cfg: &ScanI18nConfig,
) -> Vec<String> {
    let normalized = normalize_locale_tag(locale);

    // Check explicit config override first
    if let Some(cats) = cfg.locale_plural_categories.get(&normalized) {
        return cats.clone();
    }
    if let Some(cats) = cfg.locale_plural_categories.get(locale) {
        return cats.clone();
    }

    // Built-in table for common single-category locales
    match normalized.as_str() {
        // East/Southeast Asian languages — `other` only
        "zh" | "zh-hans" | "zh-hant" | "zh-cn" | "zh-tw" | "zh-hk" |
        "ja" | "ko" | "th" | "vi" | "id" | "ms" | "my" | "km" | "lo" => {
            vec!["other".to_string()]
        }
        // Turkish — `one` and `other`
        "tr" => vec!["one".to_string(), "other".to_string()],
        _ => {
            // Unknown locale → empty (conservative: warn all missing keys)
            Vec::new()
        }
    }
}

/// Normalize locale tag: lowercase, convert underscore to hyphen.
fn normalize_locale_tag(tag: &str) -> String {
    tag.to_ascii_lowercase().replace('_', "-")
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

    #[test]
    fn plural_suffix_extraction() {
        assert_eq!(extract_plural_suffix("allocation_one"), Some("one"));
        assert_eq!(extract_plural_suffix("allocation_other"), Some("other"));
        assert_eq!(extract_plural_suffix("allocation_zero"), Some("zero"));
        assert_eq!(extract_plural_suffix("allocation"), None);
        assert_eq!(extract_plural_suffix("key_with_underscore"), None);
    }

    #[test]
    fn plural_group_identification() {
        let mut keys = BTreeSet::new();
        keys.insert("models.allocation_one".into());
        keys.insert("models.allocation_other".into());
        keys.insert("models.user".into());
        let groups = identify_plural_groups(&keys);
        assert!(groups.contains("models.allocation"));
        assert!(!groups.contains("models.user"));
    }

    #[test]
    fn ms_th_missing_one_ignored() {
        let missing = vec![
            "mongodb.models.allocation_one".to_string(),
            "mongodb.models.user".to_string(),
        ];
        let mut all_keys = BTreeSet::new();
        all_keys.insert("mongodb.models.allocation_one".into());
        all_keys.insert("mongodb.models.allocation_other".into());
        all_keys.insert("mongodb.models.user".into());

        let cfg = ScanI18nConfig {
            plural_aware_filtering: true,
            ..Default::default()
        };

        let filtered_ms = filter_unsupported_plural_keys(&missing, &all_keys, "ms", &cfg);
        assert_eq!(filtered_ms, vec!["mongodb.models.user"]);

        let filtered_th = filter_unsupported_plural_keys(&missing, &all_keys, "th", &cfg);
        assert_eq!(filtered_th, vec!["mongodb.models.user"]);
    }

    #[test]
    fn locale_with_one_support_still_warns() {
        let missing = vec!["allocation_one".to_string()];
        let mut all_keys = BTreeSet::new();
        all_keys.insert("allocation_one".into());
        all_keys.insert("allocation_other".into());

        let cfg = ScanI18nConfig {
            plural_aware_filtering: true,
            ..Default::default()
        };

        // Turkish supports `one` → must warn
        let filtered = filter_unsupported_plural_keys(&missing, &all_keys, "tr", &cfg);
        assert_eq!(filtered, vec!["allocation_one"]);
    }

    #[test]
    fn unknown_locale_conservative() {
        let missing = vec!["allocation_one".to_string()];
        let mut all_keys = BTreeSet::new();
        all_keys.insert("allocation_one".into());
        all_keys.insert("allocation_other".into());

        let cfg = ScanI18nConfig {
            plural_aware_filtering: true,
            ..Default::default()
        };

        // Unknown locale "xyz" → warn everything (conservative)
        let filtered = filter_unsupported_plural_keys(&missing, &all_keys, "xyz", &cfg);
        assert_eq!(filtered, vec!["allocation_one"]);
    }

    #[test]
    fn config_override_works() {
        let missing = vec!["allocation_one".to_string()];
        let mut all_keys = BTreeSet::new();
        all_keys.insert("allocation_one".into());
        all_keys.insert("allocation_other".into());

        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("custom".to_string(), vec!["other".to_string()]);

        let cfg = ScanI18nConfig {
            plural_aware_filtering: true,
            locale_plural_categories: overrides,
            ..Default::default()
        };

        // Custom locale with override → `one` unsupported → no warning
        let filtered = filter_unsupported_plural_keys(&missing, &all_keys, "custom", &cfg);
        assert!(filtered.is_empty());
    }

    #[test]
    fn non_plural_key_always_warns() {
        let missing = vec!["mongodb.models.user".to_string()];
        let mut all_keys = BTreeSet::new();
        all_keys.insert("mongodb.models.user".into());

        let cfg = ScanI18nConfig {
            plural_aware_filtering: true,
            ..Default::default()
        };

        // Non-plural keys always pass through, regardless of locale
        let filtered_ms = filter_unsupported_plural_keys(&missing, &all_keys, "ms", &cfg);
        assert_eq!(filtered_ms, vec!["mongodb.models.user"]);

        let filtered_unknown = filter_unsupported_plural_keys(&missing, &all_keys, "xyz", &cfg);
        assert_eq!(filtered_unknown, vec!["mongodb.models.user"]);
    }
}
