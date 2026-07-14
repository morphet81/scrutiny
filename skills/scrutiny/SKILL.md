---
name: scrutiny
description: >-
  Code review skill. Prefer `scrutiny review` for script-orchestrated runs
  (headless agents, isolated|team spawn). Or chain Rust eval → map → pack → scan,
  plan-confirm, optional Task agents, review-session-write, findings triage,
  post-comments. Local default; PR URL/number for PR mode.
argument-hint: "[PR-URL | PR-number]"
---

# Scrutiny

**Preferred (no IDE agent host):** run the CLI orchestrator:

```bash
SCRUTINY_BIN="$(bash "${SKILL_ROOT}/scripts/ensure-bin.sh")"
"$SCRUTINY_BIN" review [--pr <url|number>]
```

That probes `agent`/`claude`/`codex`, asks plan knobs, runs headless review
(`isolated` parallel specialists by default, or `team` lead), triage, and posts.

This skill is for **IDE agent sessions** that still chain discrete steps below.
Complexity, map, pack, and scan stay scripts. Review agents read **pack only**.

## Usage

- `/scrutiny` — local branch vs auto-detected base (or suggest `scrutiny review`)
- `/scrutiny <PR-URL>` — PR mode
- `/scrutiny <PR-number>` — PR mode when unambiguous in current repo

## Binary

Skill root = folder that contains **this** `SKILL.md`.

**Before first command**, resolve binary:

```bash
SKILL_ROOT="<absolute-path-to-folder-containing-this-SKILL.md>"
SCRUTINY_BIN="$(bash "${SKILL_ROOT}/scripts/ensure-bin.sh")"
```

- stdout = absolute path to `scrutiny` only
- Prefer: GitHub Release **latest** (stamp `bin/.scrutiny-version`) → else `cargo build --release`. Old cache without matching stamp is refreshed.
- `ensure-bin.sh` walks up to repo `Cargo.toml` when skill lives under `skills/scrutiny/`
- Env: `SCRUTINY_GITHUB_REPO` (default `morphet81/scrutiny`). Optional `SCRUTINY_VERSION` to pin. `SCRUTINY_USE_LOCAL=1` → local target/release.
- Install skills: `"$SCRUTINY_BIN" skills-install -g -y` (wraps `npx skills add`)

Config: `~/.scrutiny/config.toml` (created on first run from shipped `config/default.toml`).
Optional: `force_client`, `force_spawn_mode` (`isolated` | `team`).

Sibling skill: `/forge` (ticket implement) — same binary.

---

## Agent workflow (mandatory order)

### 0. Mode

- No PR arg → **local**
- PR URL or number → **PR mode**
  - `gh pr view <id|url> --json baseRefName,headRefOid,headRefName,url`
  - **Never** check out PR branch into user working tree

### 1. ensure-bin → eval

```bash
SKILL_ROOT="<absolute-path-to-folder-containing-this-SKILL.md>"
SCRUTINY_BIN="$(bash "${SKILL_ROOT}/scripts/ensure-bin.sh")"
```

**Local:** `"$SCRUTINY_BIN" eval --cwd <repo-root>`  
**PR:** `"$SCRUTINY_BIN" eval --cwd <repo-root> --base <baseRefName> --head <headRefOid>`

- Prints one path: eval JSON — **show user**
- Detect client for plan: Cursor → `cursor`, Claude Code → `claude`, Codex → `codex`; else `default_client`. Pass `--client <key>` when known.

Base when `--base` omitted: `@{upstream}` → `gh` PR base → `$BASE_BRANCH` → config candidates / `origin/*`.

### 2. map

```bash
"$SCRUTINY_BIN" map --cwd <repo-root> --eval <eval-json-path>
```

Show map path. **Do not search the repo for what changed — use the map.**

### 3. pack

```bash
"$SCRUTINY_BIN" pack --cwd <repo-root> --map <map-json-path>
```

Show pack JSON path (and `markdown_path` inside JSON if present).

Pack holds:

- unified diffs for `source_to_review` / `tests_related`
- symbol slices around hunks
- doc digests (headings + first N lines)
- `needs_full_file` — **only** these paths may be full-file `Read`
- `architecture_risk` — drives evangelist spawn

**Hard rule:** review agents may **only** use pack paths / pack markdown. Forbid exploratory `Read` / `Grep` / full-file fishing unless pack marks `needs_full_file` for that path.

### 4. scan

```bash
"$SCRUTINY_BIN" scan --cwd <repo-root> --map <map-json> --pack <pack-json> --eval <eval-json>
```

Show scan path. Findings are already caveman-shaped (`title`, `explanation`, `proposed_fix`, …). **Merge these before / without AI.**

### 5. Confirm plan → plan-confirm → plan-write

**Hard rule — no chat plan prompts.** Do **not** use AskUserQuestion / multi-question chat UI for model, analyses, reviewers, or evangelists. Chat UIs cap ~4 fields per turn and split the form. Scripts own collection.

Run **one** interactive script session (all six knobs, stdin, same process):

```bash
ANSWERS="$("$SCRUTINY_BIN" plan-confirm --eval "$EVAL")"
# CI / non-interactive:
# ANSWERS="$("$SCRUTINY_BIN" plan-confirm --eval "$EVAL" --from-json '{"client":"claude","model":"opus","security":true,"performance":true,"error_handling":true,"reviewers":2,"evangelists":1}')"
```

Then write plan from answers JSON (paths from earlier steps):

```bash
PLAN="$("$SCRUTINY_BIN" plan-write \
  --eval "$EVAL" --map "$MAP" --pack "$PACK" --scan "$SCAN" \
  --answers "$ANSWERS")"
# equivalent: --from-json "$(cat "$ANSWERS")"
```

Show plan path. Read `skip_ai`, `skip_ai_reason`, `reviewers`, `evangelists`, `reviewers_requested`, `evangelists_requested`, `model`, `spawn_evangelists`, `max_reviewers`.

If `reviewers_requested` > `reviewers` (pack `max_reviewers` cap, e.g. pack_chars < 4000 → cap 1), tell user the effective count — do not spawn the raw requested number.

#### Short-circuit (no AI review)

If `skip_ai` is true (XS + docs + empty scan, or reviewers=evangelists=0):

- Print reason (e.g. “static clean; optional doc skim”)
- **Do not** spawn reviewer/evangelist agents
- Jump to **findings-init** from scan → Step 7 triage
- Optional tiny doc skim from pack digests only if user asks

### 6. AI review (when `skip_ai` is false)

#### Model application (critical)

**Cursor / Codex:** pass confirmed model to Task/Agent `model=` when the host supports it.

**Claude Code (mandatory):**

1. **Primary:** spawn every reviewer/evangelist **subagent** with Task/Agent `model: <confirmed>`  
   - Confirmed values must be Claude-valid: `haiku` / `sonnet` / `opus` or pinned ids like `claude-sonnet-4-6`  
   - Never pass Cursor slugs (`claude-4.6-sonnet-medium-thinking`, …) on the Claude path
2. **Optional session switch:** run `/model <confirmed>` once before the review turn if you need the *parent* session on that model. Document that the next user prompt may revert unless they save a default.
3. **Never claim** the parent session UI switched to the selected model unless `/model` was actually run. Say: **“review agents will use \<model\>”**.

Telling the main agent “prefer 4.6” while the UI session is Opus **does not** change the session.

#### Spawn rules (mandatory when `skip_ai` false and `plan.reviewers` > 0)

1. Partition pack paths across reviewers:

```bash
BUCKETS="$("$SCRUTINY_BIN" pack-partition --pack "$PACK" --reviewers "$(jq .reviewers "$PLAN")")"
```

`BUCKETS` = JSON array of path arrays. Reviewer *i* gets **only** `BUCKETS[i]` (+ shared plan/pack paths in the brief).

2. Spawn **exactly** `plan.reviewers` reviewer Tasks + `plan.evangelists` evangelists **in parallel**, each with confirmed `model=`.
3. **Wait for all** to finish before merge — no early stop / skim.
4. Each agent return must be structured findings with `path`+`line`, or explicit `findings: []`.
5. Lead **rejects** missing anchors / empty unexplained returns; re-spawn that agent.
6. Record session (fails if agent count ≠ expected):

```bash
SESSION="$("$SCRUTINY_BIN" review-session-write --plan "$PLAN" --pack "$PACK" --from-json "$AGENTS_JSON")"
```

`AGENTS_JSON` example: `[{"role":"reviewer","index":1,"paths":["a.ts"],"findings_count":3},…]`.

If `review-session-write` fails validation (or `agents.length != reviewers_expected + evangelists_expected`), **re-spawn missing agents** before triage — do not invent session JSON.

Other:

- Evangelists only if `plan.spawn_evangelists` and count > 0 (plan already zeroes them otherwise)
- Brief: **plan path + pack path + that agent’s path list only**
- Analyses: security / performance / error_handling from plan
- Agents must not fish outside pack / their paths

**Hard rule — anchors at raise time.** Every finding a reviewer/evangelist returns **must** include:

- `path` (repo-relative)
- `line` (1-based, from pack `symbol_slices` / diff hunk new-file lines — the agent is reading that text)
- optional `start_line`, `severity` (`critical|warning|suggestion`), `title`, `explanation`, `proposed_fix` / `fix_options`

No finding without a line. “I’ll figure out the line later” is forbidden. The lead agent must **reject and re-ask** any finding missing `path`+`line`.

Merge: static scan findings + AI findings → dedupe → write into findings JSON (Step 6.5) **with anchors already set**. For scan-only items, lead sets `anchor` from pack hunks/symbol slices when possible before showing triage.

### 6.5 findings-init (canonical findings JSON)

```bash
FINDINGS="$("$SCRUTINY_BIN" findings-init \
  --scan "$SCAN" --eval "$EVAL" --pack "$PACK" --plan "$PLAN" \
  --cwd <repo-root> [--pr <url|number>])"
```

Show findings path. **This JSON is the source of truth** — not a parallel prose list.

- Seeded from scan; then merge AI findings into the same file (renumber `F1`…, set severity)
- **Every finding must already have `anchor.path` + `anchor.line` before Step 7** (from the raising reviewer, or pack-derived for scan). Do not leave line blank hoping resolve will invent it.
- Optional `--pr` or auto `gh pr view` fills `pr_number` / `pr_url` / `head_oid`

### 7. Findings output (mandatory format — grouped by severity)

Read `$FINDINGS`. Print caveman list **grouped**. Include **`path:line`** on every item:

```
## Critical
1. Title (`src/foo.ts:42`)
   Why: …
   Fix: … | Fix options: A) … B) …

## Warning
2. …

## Suggestion
3. …
```

Each issue: **number**, **title**, **path:line**, **explanation**, **proposed fix** (options `A`, `B`, … when present). Triage order: critical → warning → suggestion.

### 8. Interactive triage → edit findings JSON → hand off to script

**Hard rule — one triage prompt.** Ask **all finding decisions in a single** multi-question form. Never split by severity. Never a second menu. **Do not** ask Request changes / Comment / Approve — that is `post-comments`'s job.

In that one form, for **each** finding `F1…Fn`:

- If it has `fix_options` → choices: each option **or Ignore**
- Else → choices: **Post** or **Ignore**

After that **one** answer set, agent work ends with file edits + starting the script:

1. Set `include` / `chosen_option` from answers
2. For each `include=true`: draft `comment_body` (why + chosen fix). Anchors already present from reviewers — do not invent lines. Script appends `[AI Agent]` if missing.
3. Leave `review.event` unset (or null)
4. Verify anchors:

```bash
"$SCRUTINY_BIN" findings-resolve --findings "$FINDINGS" --cwd <repo-root>
```

5. If `line_resolved=false` on an included finding: fix from pack/head (real cited line), resolve again. Critical must resolve.
6. **Stop agent prompting.** Run the poster. Requires PR — else stop with “open a PR or re-run `/scrutiny <pr-url>`”:

```bash
"$SCRUTINY_BIN" findings-validate --findings "$FINDINGS"
RESULT="$("$SCRUTINY_BIN" post-comments --findings "$FINDINGS" --cwd <repo-root>")"
```

Optional non-interactive: `post-comments --event COMMENT|REQUEST_CHANGES|APPROVE`.

**`post-comments` owns GitHub review API.** Script prompts for `COMMENT` / `REQUEST_CHANGES` / `APPROVE`. If your user already has a **PENDING** review, script asks add-vs-close (then event). Agent must **never** run `gh api` to create / dismiss / delete / submit reviews. If script fails, show stderr to user — do not improvise.

Show result path / review `html_url` from the script output. Agent must **not** re-ask the review action in chat.

---

## Notes

- Pipeline: `ensure-bin` → `eval` → `map` → `pack` → `scan` → `plan-confirm` → `plan-write` → (optional AI: partition + parallel spawn + wait + `review-session-write`) → `findings-init` → **one** triage prompt → `findings-resolve` → `post-comments` (pending + event prompts)
- Edit `~/.scrutiny/config.toml` for models / pack / scan / agent counts
- Claude `[models.claude]` uses aliases or pinned Anthropic ids only — not Cursor slugs
- Install: `npx skills add <owner>/scrutiny -g -y --skill '*'` (see README)
