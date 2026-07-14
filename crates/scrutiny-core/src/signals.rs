//! Deterministic content signals for security / performance / error-handling knobs.

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::{Config, ReviewSignalsConfig};
use crate::eval::EvalFile;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContentSignals {
    pub security: bool,
    pub performance: bool,
    pub error_handling: bool,
    pub security_reason: String,
    pub performance_reason: String,
    pub error_handling_reason: String,
    pub security_hits: Vec<String>,
    pub performance_hits: Vec<String>,
    pub error_handling_hits: Vec<String>,
}

pub fn detect_content_signals(
    cfg: &Config,
    files: &[EvalFile],
    unified_diff: &str,
) -> ContentSignals {
    let sig = &cfg.review.signals;
    if sig.ignore_content_signals {
        return ContentSignals {
            security: true,
            performance: true,
            error_handling: true,
            security_reason: "content signals ignored (config)".into(),
            performance_reason: "content signals ignored (config)".into(),
            error_handling_reason: "content signals ignored (config)".into(),
            ..Default::default()
        };
    }

    let scored: Vec<&EvalFile> = files
        .iter()
        .filter(|f| f.kind != "doc" && f.kind != "i18n")
        .collect();

    let mut out = ContentSignals::default();

    // --- security ---
    let mut sec_hits = Vec::new();
    for f in &scored {
        if f.risk {
            sec_hits.push(format!("risk_path:{}", f.path));
        }
        if path_matches_any(&f.path, &sig.security_path_globs) {
            sec_hits.push(format!("path:{}", f.path));
        }
    }
    sec_hits.extend(diff_pattern_hits(unified_diff, &sig.security_diff_patterns));

    let only_skip_kinds = !scored.is_empty()
        && scored.iter().all(|f| {
            is_ui_presentational(f) || f.kind == "test" || f.kind == "doc" || f.kind == "i18n"
        })
        && sec_hits.is_empty();

    if only_skip_kinds {
        out.security = false;
        out.security_reason = "UI/test/docs only — no network/auth/storage sinks".into();
    } else if sec_hits.is_empty() {
        out.security = false;
        out.security_reason = "no network/auth/storage/injection signals".into();
    } else {
        out.security = true;
        out.security_hits = uniq(sec_hits);
        out.security_reason = format!("hit: {}", out.security_hits.first().unwrap());
    }

    // --- performance ---
    let mut perf_hits = Vec::new();
    for f in &scored {
        if path_matches_any(&f.path, &sig.performance_path_globs) {
            perf_hits.push(format!("path:{}", f.path));
        }
        if path_matches_any(&f.path, &sig.performance_css_path_globs)
            && diff_has_any(unified_diff, &sig.performance_css_patterns)
        {
            perf_hits.push(format!("css:{}", f.path));
        }
    }
    perf_hits.extend(diff_pattern_hits(unified_diff, &sig.performance_diff_patterns));

    // CSS-only presentational without hot patterns → no perf
    let css_only = !scored.is_empty()
        && scored.iter().all(|f| {
            let p = f.path.to_ascii_lowercase();
            p.ends_with(".css")
                || p.ends_with(".scss")
                || p.ends_with(".sass")
                || p.ends_with(".less")
                || f.kind == "test"
                || is_ui_presentational(f)
        });

    if css_only && !diff_has_any(unified_diff, &sig.performance_css_patterns) {
        out.performance = false;
        out.performance_reason = "presentational UI/CSS — no hot-path/complex CSS patterns".into();
    } else if perf_hits.is_empty() {
        out.performance = false;
        out.performance_reason = "no hooks/domain/loops/layout thrash signals".into();
    } else {
        out.performance = true;
        out.performance_hits = uniq(perf_hits);
        out.performance_reason = format!("hit: {}", out.performance_hits.first().unwrap());
    }

    // --- error handling ---
    let mut err_hits = diff_pattern_hits(unified_diff, &sig.error_handling_diff_patterns);
    for f in &scored {
        let p = f.path.to_ascii_lowercase();
        if p.contains("error") || p.contains("catch") || p.contains("retry") {
            err_hits.push(format!("path:{}", f.path));
        }
    }
    if err_hits.is_empty() {
        out.error_handling = false;
        out.error_handling_reason = "no async/Result/try/catch signals".into();
    } else {
        out.error_handling = true;
        out.error_handling_hits = uniq(err_hits);
        out.error_handling_reason = format!("hit: {}", out.error_handling_hits.first().unwrap());
    }

    let _ = sig; // silence if unused fields later
    out
}

fn is_ui_presentational(f: &EvalFile) -> bool {
    let p = f.path.replace('\\', "/").to_ascii_lowercase();
    (p.contains("/components/") || f.layer.as_deref() == Some("ui"))
        && !p.contains("/hooks/")
        && f.layer.as_deref() != Some("hooks")
        && f.layer.as_deref() != Some("domain")
        && f.layer.as_deref() != Some("stores")
        && f.layer.as_deref() != Some("data")
}

fn path_matches_any(path: &str, globs: &[String]) -> bool {
    let p = path.replace('\\', "/");
    let lower = p.to_ascii_lowercase();
    for g in globs {
        if glob_match(&lower, &g.to_ascii_lowercase()) {
            return true;
        }
    }
    false
}

/// Minimal glob: `**` / `*` and literal substrings like `**/*auth*`.
fn glob_match(path: &str, pattern: &str) -> bool {
    if let Ok(g) = globset::Glob::new(pattern) {
        return g.compile_matcher().is_match(path);
    }
    // fallback substring for broken patterns
    let stripped = pattern.replace("**/", "").replace("*", "");
    !stripped.is_empty() && path.contains(&stripped)
}

fn diff_has_any(diff: &str, patterns: &[String]) -> bool {
    !diff_pattern_hits(diff, patterns).is_empty()
}

fn diff_pattern_hits(diff: &str, patterns: &[String]) -> Vec<String> {
    let mut hits = Vec::new();
    // Only scan added lines
    let mut added = String::new();
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added.push_str(&line[1..]);
            added.push('\n');
        }
    }
    for pat in patterns {
        match Regex::new(pat) {
            Ok(re) => {
                if re.is_match(&added) {
                    hits.push(format!("diff:/{pat}/"));
                }
            }
            Err(_) => continue,
        }
    }
    hits
}

fn uniq(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v.truncate(12);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> Config {
        toml::from_str(crate::config::DEFAULT_TOML).unwrap()
    }

    #[test]
    fn ui_only_no_security() {
        let files = vec![EvalFile {
            path: "src/components/Foo/Foo.tsx".into(),
            status: "M".into(),
            added: 10,
            deleted: 0,
            kind: "source".into(),
            layer: Some("ui".into()),
            risk: false,
            blast_stub: 0,
        }];
        let diff = "@@\n+const x = clsx('a', 'b');\n";
        let s = detect_content_signals(&cfg(), &files, diff);
        assert!(!s.security, "{:?}", s.security_reason);
    }

    #[test]
    fn fetch_triggers_security() {
        let files = vec![EvalFile {
            path: "src/api/client.ts".into(),
            status: "M".into(),
            added: 5,
            deleted: 0,
            kind: "source".into(),
            layer: Some("source".into()),
            risk: false,
            blast_stub: 0,
        }];
        let diff = "@@\n+await fetch('/api/x');\n";
        let s = detect_content_signals(&cfg(), &files, diff);
        assert!(s.security, "{:?}", s);
    }

    #[test]
    fn hooks_trigger_performance() {
        let files = vec![EvalFile {
            path: "src/hooks/useThing.ts".into(),
            status: "M".into(),
            added: 20,
            deleted: 0,
            kind: "source".into(),
            layer: Some("hooks".into()),
            risk: false,
            blast_stub: 2,
        }];
        let diff = "@@\n+useEffect(() => { items.map(x => x); }, []);\n";
        let s = detect_content_signals(&cfg(), &files, diff);
        assert!(s.performance, "{:?}", s);
    }
}

/// Compile-time check config shape is used.
#[allow(dead_code)]
fn _use_signals_cfg(s: &ReviewSignalsConfig) -> bool {
    s.ignore_content_signals
}
