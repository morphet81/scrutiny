use crate::score::Tier;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathKind {
    Source,
    Test,
    Doc,
    Noise,
    Config,
    /// Locale / translation files — deterministic scan only; not AI-scored.
    I18n,
    Other,
}

pub fn classify_path(path: &str) -> PathKind {
    let lower = path.to_ascii_lowercase();
    if is_doc(&lower) {
        return PathKind::Doc;
    }
    if is_i18n(&lower) {
        return PathKind::I18n;
    }
    if is_test(&lower) {
        return PathKind::Test;
    }
    if is_config(&lower) {
        return PathKind::Config;
    }
    if looks_source(&lower) {
        return PathKind::Source;
    }
    PathKind::Other
}

/// True for paths that should not inflate AI review tier / pack.
pub fn excluded_from_score(kind: &PathKind) -> bool {
    matches!(kind, PathKind::Doc | PathKind::I18n | PathKind::Noise)
}

fn is_doc(p: &str) -> bool {
    p.ends_with(".md")
        || p.ends_with(".mdx")
        || p.ends_with(".rst")
        || p.ends_with(".txt")
        || p.contains("/docs/")
        || p.starts_with("docs/")
        || p.contains("/lore/")
        || p.starts_with("lore/")
        || p.ends_with("/meta.md")
        || p.ends_with("readme")
        || p.ends_with("readme.md")
        || p.contains("/.skills/")
        || p.contains("skill.md")
}

pub fn is_i18n(p: &str) -> bool {
    let p = p.replace('\\', "/");
    p.contains("/locales/")
        || p.contains("/locale/")
        || p.contains("/i18n/locales/")
        || p.contains("/lang/")
        || p.contains("/translations/")
        || p.ends_with(".po")
        || p.ends_with(".pot")
        || p.ends_with(".xliff")
        || p.ends_with(".xlf")
}

fn is_test(p: &str) -> bool {
    p.contains("/__tests__/")
        || p.contains("/tests/")
        || p.contains(".specs.")
        || p.contains(".spec.")
        || p.contains(".test.")
        || p.ends_with("_test.go")
        || p.ends_with("_test.rs")
        || p.ends_with(".test.ts")
        || p.ends_with(".test.tsx")
        || p.ends_with(".specs.tsx")
        || p.ends_with(".specs.ts")
}

fn is_config(p: &str) -> bool {
    p.ends_with(".toml")
        || p.ends_with(".yaml")
        || p.ends_with(".yml")
        || (p.ends_with(".json") && !is_i18n(p))
        || p.ends_with(".lock")
        || p == "dockerfile"
        || p.ends_with("/dockerfile")
}

fn looks_source(p: &str) -> bool {
    p.ends_with(".rs")
        || p.ends_with(".ts")
        || p.ends_with(".tsx")
        || p.ends_with(".js")
        || p.ends_with(".jsx")
        || p.ends_with(".py")
        || p.ends_with(".go")
        || p.ends_with(".java")
        || p.ends_with(".kt")
        || p.ends_with(".swift")
        || p.ends_with(".scss")
        || p.ends_with(".css")
        || p.ends_with(".c")
        || p.ends_with(".cpp")
        || p.ends_with(".h")
}

pub fn layer_for_path(path: &str) -> Option<&'static str> {
    let p = path.replace('\\', "/");
    let lower = p.to_ascii_lowercase();
    if is_doc(&lower) {
        return Some("docs");
    }
    if is_i18n(&lower) {
        return Some("i18n");
    }
    if p.contains("/domain/") || p.starts_with("src/domain/") {
        return Some("domain");
    }
    if p.contains("/data/") || p.starts_with("src/data/") {
        return Some("data");
    }
    if p.contains("/routes/") || p.starts_with("src/routes/") {
        return Some("routes");
    }
    if p.contains("/components/") || p.starts_with("src/components/") {
        return Some("ui");
    }
    if p.contains("/hooks/") || p.starts_with("src/hooks/") {
        return Some("hooks");
    }
    if p.contains("/stores/") || p.starts_with("src/stores/") {
        return Some("stores");
    }
    if p.contains("/native/") || p.contains("/android/") || p.contains("/ios/") {
        return Some("native");
    }
    if looks_source(&lower) {
        return Some("source");
    }
    None
}

pub fn is_risk_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    // Avoid false positives on "token" inside i18n copy files
    if is_i18n(&p) {
        return false;
    }
    p.contains("auth")
        || p.contains("permission")
        || p.contains("security")
        || p.contains("csp")
        || p.contains("password")
        || p.contains("oauth")
        || p.contains("payment")
        || p.contains("billing")
        || p.contains("migrat")
        || p.contains("crypto")
        || p.contains("secret")
        || (p.contains("token") && (p.contains("auth") || p.contains("jwt") || p.contains("session")))
}

pub fn change_class(kinds: &[PathKind]) -> String {
    let has_source = kinds
        .iter()
        .any(|k| matches!(k, PathKind::Source | PathKind::Test | PathKind::Config));
    let has_doc = kinds.iter().any(|k| matches!(k, PathKind::Doc));
    let only_i18n = !kinds.is_empty()
        && kinds
            .iter()
            .all(|k| matches!(k, PathKind::I18n | PathKind::Doc));
    if only_i18n && !has_source {
        return if has_doc { "docs".into() } else { "i18n".into() };
    }
    match (has_source, has_doc) {
        (false, true) => "docs".into(),
        (true, false) => "source".into(),
        (true, true) => "mixed".into(),
        (false, false) => "other".into(),
    }
}

pub fn suggested_scope_for_tier(tier: Tier) -> Vec<String> {
    match tier {
        Tier::Xs => vec![
            "Skim docs / tiny diffs only".into(),
            "Skip deep security unless risk paths hit".into(),
        ],
        Tier::S => vec![
            "Review changed source files".into(),
            "Check related tests".into(),
        ],
        Tier::M => vec![
            "Review source + tests".into(),
            "Check error handling on touched paths".into(),
            "Light security pass if risk tags present".into(),
        ],
        Tier::L => vec![
            "Full source review across layers".into(),
            "Security + error handling".into(),
            "Performance on hot paths".into(),
        ],
        Tier::Xl => vec![
            "Multi-agent deep review".into(),
            "Security, performance, error handling".into(),
            "Consider split if too tangled".into(),
        ],
    }
}

/// Blast score for one path. Base is **0** unless a boost rule matches.
/// Never use sum-of-ones across files — that inflate tier on locale fan-out.
pub fn blast_stub_for_path(path: &str) -> u32 {
    let p = path.replace('\\', "/");
    let lower = p.to_ascii_lowercase();
    if is_i18n(&lower) || is_doc(&lower) {
        return 0;
    }
    let mut score = 0u32;
    if p.contains("/domain/") || p.contains("/data/schemas/") || p.contains("/stores/") {
        score += 8;
    }
    if p.contains("/utils/") || p.contains("/lib/") || p.ends_with("/mod.rs") || p.ends_with("/index.ts")
    {
        score += 4;
    }
    if p.contains("/routes/") || p.contains("__root") {
        score += 5;
    }
    if p.contains("/components/atoms/") {
        score += 3;
    }
    if p.contains("/hooks/") {
        score += 2;
    }
    score
}

/// Aggregate per-file blast: max boost + small count of boosted paths (not sum of all).
pub fn aggregate_blast(per_file: &[u32]) -> u32 {
    let max = per_file.iter().copied().max().unwrap_or(0);
    let boosted = per_file.iter().filter(|&&b| b > 0).count() as u32;
    max.saturating_add(boosted.saturating_sub(1).saturating_mul(2)).min(40)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_meta_as_doc() {
        assert_eq!(
            classify_path("src/components/Toast/META.md"),
            PathKind::Doc
        );
    }

    #[test]
    fn classifies_specs_as_test() {
        assert_eq!(
            classify_path("src/components/Toast/Toast.specs.tsx"),
            PathKind::Test
        );
    }

    #[test]
    fn classifies_locale_json_as_i18n() {
        assert_eq!(
            classify_path("src/i18n/locales/en.json"),
            PathKind::I18n
        );
        assert_eq!(
            classify_path("src/locales/ja.json"),
            PathKind::I18n
        );
    }

    #[test]
    fn risk_auth() {
        assert!(is_risk_path("src/hooks/useAuth.ts"));
        assert!(!is_risk_path("src/i18n/locales/en.json"));
    }

    #[test]
    fn blast_no_base_one() {
        assert_eq!(blast_stub_for_path("src/components/Foo/Foo.tsx"), 0);
        assert!(blast_stub_for_path("src/hooks/useX.ts") >= 2);
        assert_eq!(blast_stub_for_path("src/i18n/locales/en.json"), 0);
    }

    #[test]
    fn aggregate_blast_not_sum_ones() {
        // 23 locale-like zeros + a few boosted
        let mut v = vec![0u32; 23];
        v.push(8);
        v.push(2);
        assert!(aggregate_blast(&v) < 30);
        // Old bug: 40 files * 1 = 40 → max bucket
        assert_eq!(aggregate_blast(&vec![0; 40]), 0);
    }
}
