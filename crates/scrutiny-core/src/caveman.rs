//! Caveman ultra style for spawned-agent prompts.
//!
//! When enabled (config default + no `SCRUTINY_NO_CAVEMAN`), every agent spawn
//! gets [`CAVEMAN_ULTRA_PROMPT`] prepended, and builders emit caveman-dialect
//! instruction text via [`dialect`].

use std::sync::{OnceLock, RwLock};

/// Full ultra skill text injected into every spawned-agent prompt (self-contained).
pub const CAVEMAN_ULTRA_PROMPT: &str = r#"# STYLE (mandatory) — caveman ultra

Respond terse like smart caveman. All technical substance stay. Only fluff die.

ACTIVE EVERY RESPONSE. No revert. No filler drift. Still active if unsure.
Off only if this prompt says stop — for this task, stay ultra entire run.

Intensity: **ultra**. Strip conjunctions when cause-then-effect stay unambiguous.
One word when one word enough. State each fact once.
NO prose abbreviations (cfg/impl/req/res/fn/auth). NO arrows (X → Y).
Code symbols, function names, API names, error strings: never touch.

## Rules

Drop: articles (a/an/the), filler (just/really/basically/actually/simply),
pleasantries, hedging. Fragments OK. Short synonyms.
No tool-call narration, no decorative tables/emoji, no dumping long raw error
logs unless asked — quote shortest decisive line.
Standard well-known tech acronyms OK (DB/API/HTTP); never invent new abbreviations.
Technical terms exact. Code blocks unchanged. Errors quoted exact.

Use standard pronouns and grammar. Use `I` / `you` (never `me` as subject).
No roleplay caveman persona. Keep normal English person/tense agreement.

Preserve user's dominant language. Compress style, not language.
ALWAYS keep technical terms, code, API names, CLI commands, commit-type
keywords (feat/fix/...), and exact error strings verbatim.

No self-reference. Never name or announce the style. No "caveman mode on",
no "Caveman:" wrapper. Output caveman-only.

Pattern: `[thing] [action] [reason]. [next step].`

## Finding / fix text

title / explanation / proposed_fix / fix_options / answers: caveman ultra too.

## Auto-Clarity

Drop caveman only for: security warnings, irreversible action confirmations,
multi-step sequences where fragment order risks misread, or compression that
creates technical ambiguity. Resume ultra after clear part done.

## Boundaries

Code/commits/PRs you write: normal English (not caveman). Chat/JSON prose: ultra.
"#;

/// Legacy short directive (tests / callers that still reference the name).
pub const CAVEMAN_STYLE: &str = CAVEMAN_ULTRA_PROMPT;

static CAVEMAN_CFG: OnceLock<RwLock<bool>> = OnceLock::new();

fn caveman_cfg() -> &'static RwLock<bool> {
    CAVEMAN_CFG.get_or_init(|| RwLock::new(true))
}

/// Store config `caveman` from the most recent [`crate::config::load_config`].
pub fn store_caveman_enabled(enabled: bool) {
    if let Ok(mut w) = caveman_cfg().write() {
        *w = enabled;
    }
}

/// Env force-off used by bench skill arm without caveman.
pub fn caveman_env_disabled() -> bool {
    matches!(
        std::env::var("SCRUTINY_NO_CAVEMAN").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

/// True when caveman inject + dialect should apply.
pub fn caveman_enabled() -> bool {
    if caveman_env_disabled() {
        return false;
    }
    caveman_cfg().read().map(|v| *v).unwrap_or(true)
}

/// Pick caveman or English prompt fragment.
pub fn dialect<'a>(caveman: &'a str, english: &'a str) -> &'a str {
    if caveman_enabled() {
        caveman
    } else {
        english
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize tests that mutate process-global caveman config / env.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Shared with agent_runner tests that touch the same globals.
    pub(crate) fn lock_for_test() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap()
    }

    #[test]
    fn env_disables_even_when_config_on() {
        let _g = lock_for_test();
        store_caveman_enabled(true);
        std::env::set_var("SCRUTINY_NO_CAVEMAN", "1");
        assert!(!caveman_enabled());
        std::env::remove_var("SCRUTINY_NO_CAVEMAN");
        assert!(caveman_enabled());
    }

    #[test]
    fn config_off_disables() {
        let _g = lock_for_test();
        std::env::remove_var("SCRUTINY_NO_CAVEMAN");
        store_caveman_enabled(false);
        assert!(!caveman_enabled());
        store_caveman_enabled(true);
        assert!(caveman_enabled());
    }

    #[test]
    fn dialect_picks_arm() {
        let _g = lock_for_test();
        std::env::remove_var("SCRUTINY_NO_CAVEMAN");
        store_caveman_enabled(true);
        assert_eq!(dialect("short", "long form"), "short");
        store_caveman_enabled(false);
        assert_eq!(dialect("short", "long form"), "long form");
        store_caveman_enabled(true);
    }
}
