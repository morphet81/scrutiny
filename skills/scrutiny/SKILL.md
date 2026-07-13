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
- Prefer: `bin/scrutiny` → `target/release/scrutiny` → GitHub Release **latest** → `cargo build --release`
- `ensure-bin.sh` walks up to repo `Cargo.toml` when skill lives under `skills/scrutiny/`
- Env: `SCRUTINY_GITHUB_REPO` (default `morphet81/scrutiny`). Optional `SCRUTINY_VERSION` to pin (default = latest release).

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

Read eval `tier` + `suggested_plan` (do **not** re-parse whole config into prompts). Ask user to confirm:

| Prompt | Default | Hide rule |
|--------|---------|-----------|
| Model | `suggested_plan.model` | always show |
| Security | `suggested_plan.security` | always show |
| Performance | `suggested_plan.performance` | always show |
| Error handling | `suggested_plan.error_handling` | always show |
| Reviewer agents | `suggested_plan.reviewers` | hide if `prompt_reviewers` false → 0 |
| Evangelists | `suggested_plan.evangelists` | hide if `prompt_evangelists` false → 0 |

Then write plan (no re-load of config prose later):

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

Merge: static scan findings + AI findings → dedupe → numbered caveman list.

### 6.5 findings-init (canonical findings JSON)

```bash
FINDINGS="$("$SCRUTINY_BIN" findings-init \
  --scan "$SCAN" --eval "$EVAL" --pack "$PACK" --plan "$PLAN" \
  --cwd <repo-root> [--pr <url|number>])"
```

Show findings path. **This JSON is the source of truth** — not a parallel prose list.

- Seeded from scan findings (severity already `critical|warning|info`)
- Merge AI findings into the same file: add items, renumber `number`/`id` (`F1`…), set `severity`, `paths`, draft `anchor.path` + `anchor.line` from pack symbol slices / diff hunks only
- Optional `--pr` or auto `gh pr view` fills `pr_number` / `pr_url` / `head_oid`

### 7. Findings output (mandatory format — grouped by severity)

Read `$FINDINGS`. Print caveman list **grouped**:

```
## Critical
1. Title
   Why: …
   Fix: … | Fix options: A) … B) …

## Warning
2. …

## Info
3. …
```

Each issue: **number**, **title**, **explanation**, **proposed fix** (options `A`, `B`, … when present).

### 8. Interactive triage → edit findings JSON

Exact order. After each answer, **write** the findings file (do not only remember in chat):

1. Multi-option findings → set `chosen_option` (`A`/`B`/…) **or** `include=false` (Ignore)
2. Checkbox list of single-fix findings → set `include` true/false for each
3. For each `include=true`:
   - Draft `comment_body` (why + chosen fix). Script appends `[AI Agent]` if missing.
   - Set `anchor.path` + `anchor.line` (and optional `start_line` / `needle`) from pack only — **never invent line numbers**
4. Resolve anchors against PR/branch head blob:

```bash
"$SCRUTINY_BIN" findings-resolve --findings "$FINDINGS" --cwd <repo-root>
```

5. If `line_resolved=false` on included findings: re-read `git show <head_oid>:<path>`, fix `line`/`needle`, resolve again. Critical included must resolve.
6. Ask review action → set `review.event` + short `review.body` (counts of included critical/warning/info):
   - **Request changes** → `REQUEST_CHANGES`
   - **Comment only** → `COMMENT`
   - **Approve** → `APPROVE`
7. Validate + post (requires PR — else stop: open a PR or re-run `/scrutiny <pr-url>`):

```bash
"$SCRUTINY_BIN" findings-validate --findings "$FINDINGS"
RESULT="$("$SCRUTINY_BIN" post-comments --findings "$FINDINGS" --cwd <repo-root>)"
```

Show result path / review `html_url`. Comments post as one PR review; each line comment and the review body end with `[AI Agent]`.

---

## Notes

- Pipeline: `ensure-bin` → `eval` → `map` → `pack` → `scan` → confirm → `plan-write` → (optional AI) → `findings-init` → triage → `findings-resolve` → `findings-validate` → `post-comments`
- Edit `~/.scrutiny/config.toml` for models / pack / scan / agent counts
- Claude `[models.claude]` uses aliases or pinned Anthropic ids only — not Cursor slugs
- Install: `npx skills add <owner>/scrutiny -g -y --skill '*'` (see README)
