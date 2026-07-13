---
name: scrutiny
description: >-
  Code review skill. Runs Rust eval → map → pack → scan, confirms plan,
  optionally spawns review agents on pack slices only (Claude: Task model=),
  merges static+AI findings, caveman list, interactive triage. Local default;
  PR URL/number for PR mode.
argument-hint: "[PR-URL | PR-number]"
---

# Scrutiny

Code review skill. Complexity, map, pack, and scan are **scripts** (Rust).
Agent judgment starts after artifacts exist. Review agents read **pack only**.

## Usage

- `/scrutiny` — local branch vs auto-detected base
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

Config: `~/.scrutiny/config.toml` (created on first run from shipped `config/default.toml`).

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

### 5. Confirm plan → plan-write

Read eval `tier` + `suggested_plan` (do **not** re-parse whole config into prompts).

**Hard rule — one turn, separate fields.** Ask **all** of the prompts below in a **single** multi-question UI / one message (every applicable field at once). Never split into a second round (e.g. model+analyses first, reviewers later). Never bundle into combined presets like “opus, all on” — each field stays its own question with its own choices.

Include every row whose Hide rule does not apply:

| # | Prompt | Choices | Default |
|---|--------|---------|---------|
| 1 | **Model** | Exactly `suggested_plan.available_models` (all ids for this client). Mark recommended. | `suggested_plan.model` |
| 2 | **Security analysis?** | yes / no | `suggested_plan.security` |
| 3 | **Performance analysis?** | yes / no | `suggested_plan.performance` |
| 4 | **Error-handling analysis?** | yes / no | `suggested_plan.error_handling` |
| 5 | **Reviewer agents** (count) | 0,1,2,… | `suggested_plan.reviewers` — **omit from the form** if `prompt_reviewers` false → use `0` |
| 6 | **Evangelists** (count) | 0,1,2,… | `suggested_plan.evangelists` — **omit** if `prompt_evangelists` false → use `0` |

Model field: list every entry in `available_models` (e.g. haiku, sonnet, claude-sonnet-4-6, opus), default = recommended `model`. Do **not** shrink the list to two presets.

Wait for that one answer set, then write plan:

```bash
"$SCRUTINY_BIN" plan-write \
  --eval <eval> --map <map> --pack <pack> --scan <scan> \
  --client <client> --model <confirmed-model> \
  --security <true|false> --performance <true|false> --error-handling <true|false> \
  --reviewers <n> --evangelists <n>
```

Show plan path. Read `skip_ai`, `skip_ai_reason`, `reviewers`, `evangelists`, `model`, `spawn_evangelists`, `max_reviewers`.

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

#### Spawn rules

- Reviewer count = `plan.reviewers` (already capped by pack size via `max_reviewers`)
- Evangelists only if `plan.spawn_evangelists` (architecture_risk or tier ≥ L) and count > 0
- Brief each agent with: **plan.json + pack path only** (not full eval/config dumps)
- Enabled analyses only: security / performance / error_handling from plan
- Agents must not fish outside pack

**Hard rule — anchors at raise time.** Every finding a reviewer/evangelist returns **must** include:

- `path` (repo-relative)
- `line` (1-based, from pack `symbol_slices` / diff hunk new-file lines — the agent is reading that text)
- optional `start_line`, `severity` (`critical|warning|info`), `title`, `explanation`, `proposed_fix` / `fix_options`

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

## Info
3. …
```

Each issue: **number**, **title**, **path:line**, **explanation**, **proposed fix** (options `A`, `B`, … when present).

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
6. **Stop agent prompting.** Run the poster (it asks review action on stdin, then posts). Requires PR — else stop with “open a PR or re-run `/scrutiny <pr-url>`”:

```bash
"$SCRUTINY_BIN" findings-validate --findings "$FINDINGS"
RESULT="$("$SCRUTINY_BIN" post-comments --findings "$FINDINGS" --cwd <repo-root>")"
```

Optional non-interactive: `post-comments --event COMMENT|REQUEST_CHANGES|APPROVE`.

Show result path / review `html_url` from the script output. Agent must **not** re-ask the review action in chat.

---

## Notes

- Pipeline: `ensure-bin` → `eval` → `map` → `pack` → `scan` → confirm → `plan-write` → (optional AI with anchors) → `findings-init` → **one** triage prompt → `findings-resolve` → `post-comments` (script prompts review event + posts)
- Edit `~/.scrutiny/config.toml` for models / pack / scan / agent counts
- Claude `[models.claude]` uses aliases or pinned Anthropic ids only — not Cursor slugs
- Install: `npx skills add <owner>/scrutiny -g -y --skill '*'` (see README)
