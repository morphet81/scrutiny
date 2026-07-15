---
name: parley
description: >-
  Address unresolved GitHub PR review comments. Prefer `scrutiny parley`
  (fetch threads, knobs, isolated|team fix agents, optional evangelist verify,
  host commit/push, script thread replies). Or chain parley-fetch / plan-write /
  reply.
argument-hint: "[PR-URL | PR-number]"
---

# Parley

**Preferred (script-orchestrated):**

```bash
SKILL_ROOT="<absolute-path-to-folder-containing-this-SKILL.md>"
SCRUTINY_BIN="$(bash "${SKILL_ROOT}/scripts/ensure-bin.sh")"
"$SCRUTINY_BIN" parley [--pr <url|number>]
```

Fetches unresolved review threads (GraphQL), asks members / evangelists /
spawn mode, runs fix agents, host commits + pushes, then posts a reply under
each thread via `addPullRequestReviewThreadReply`.

Sibling of `/scrutiny` and `/forge` (same binary, `~/.scrutiny/config.toml`).

## Usage

- `/parley` — current branch PR
- `/parley <PR-URL>` — specified PR
- `/parley <PR-number>` — specified PR when unambiguous

## Binary

Skill root = folder containing **this** `SKILL.md`.

```bash
SKILL_ROOT="<absolute-path-to-folder-containing-this-SKILL.md>"
SCRUTINY_BIN="$(bash "${SKILL_ROOT}/scripts/ensure-bin.sh")"
```

Config: `~/.scrutiny/config.toml` → root `headless`, `[parley]` (`default_members`,
`default_verifiers`, `default_evangelists`).

**Artifacts:** `<repo>/.scrutiny/<pr>/parley-comments.json`, `parley-plan.json`,
`parley-fixes.json`, `parley-reply.json`, `parley-session.json`.

## Knobs

| Prompt | Default | Rule |
|--------|---------|------|
| Team members | 1 | Cannot exceed unresolved comment count |
| Verifiers (check fixes) | 1 | Both modes; runs after fixes, before evangelist |
| Evangelists (verify after) | 1 | Isolated mode only for separate post-pass |
| Spawn mode | `isolated` | `isolated` \| `team` |

Team mode: lead embeds verify when evangelists > 0 (no second wave).

**Verifier** (flag-only): confirms each `addressed:true` fix truly resolves its
comment (else flips `addressed:false`), and each `addressed:false` reply is
consistent. Writes `verified`/`verification` into `parley-fixes.json`; does not
edit source code. N>1 verifiers each check all threads (redundant).

## Headless vs windows

Config root `headless` (default `true`). Set `headless = false` to open each
agent in a visible terminal window in claude **auto** mode
(`--permission-mode auto`), on the current surface: tmux/zellij (new session/
pane), macOS Terminal/iTerm2 (new window). Agents write fixes to disk; the host
polls a per-agent done sentinel. Non-claude clients or unsupported surfaces
(plain SSH/CI) fall back to headless.

## Hard rules

- Agents **must not** `git commit` / `push` / `gh` reply — host owns ship + reply
- Every thread gets a `parley-fixes.json` entry (`comment_id` = thread id `PRRT_…`)
- Replies use GraphQL `addPullRequestReviewThreadReply` with that thread id
- No auto-resolve in v1

## Push failure recovery

Host tees `git push` to `.scrutiny/<pr>/push-attempt-N.log`. On hook/test failure,
spawns `parley-push-fix` (up to `[parley].push_fix_max_loops`, default 2), host
commits `fix: repair pre-push failures`, retries push. Auth/remote errors skip agent.

## Discrete steps (IDE chaining)

```bash
COMMENTS="$("$SCRUTINY_BIN" parley-fetch --cwd <repo> [--pr <n>])"
PLAN="$("$SCRUTINY_BIN" parley-plan-write --comments "$COMMENTS" --from-json '{...}')"
# … agents update parley-fixes.json …
"$SCRUTINY_BIN" parley-reply --fixes <parley-fixes.json> --cwd <repo>
```

Prefer jump straight to `"$SCRUTINY_BIN" parley`.
