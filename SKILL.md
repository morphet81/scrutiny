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
- Prefer: `bin/scrutiny` → `target/release/scrutiny` → GitHub Release → `cargo build --release`
- Env: `SCRUTINY_GITHUB_REPO` (default `alexanderobellianne/scrutiny`), `SCRUTINY_VERSION`

Config: `~/.scrutiny/config.toml` (created on first run from shipped `config/default.toml`).

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
- Jump to findings from **scan only** → Step 7 triage
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

### 7. Findings output (mandatory format)

Clear, concise, caveman-style numbered list. Each issue:

1. **Number**
2. **Title**
3. **Explanation** — short
4. **Proposed fix** — multi-fix → options `A`, `B`, …

```
1. Missing null guard on reservation id
   Why: `id` can be undefined after fetch; crash on open.
   Fix: Guard before use; show empty state if missing.

2. N+1 fetch in list render
   Why: Each card hits store in loop.
   Fix options:
   A) Batch load once in parent
   B) Derive from already-loaded collection
```

### 8. Interactive choices (exact order)

1. Findings with multi-option fixes → ask option **or Ignore**
2. Checkbox list of all other (single-fix) findings → which to report
3. Final report = chosen multi-option + checked singles only

---

## Notes

- Pipeline: `ensure-bin` → `eval` → `map` → `pack` → `scan` → confirm → `plan-write` → (optional AI) → merge → triage
- Edit `~/.scrutiny/config.toml` for models / pack / scan / agent counts
- Claude `[models.claude]` uses aliases or pinned Anthropic ids only — not Cursor slugs
- Install: `npx skills add <owner>/scrutiny -g -y` (see README)
