---
name: forge
description: >-
  Implement a ticket or description with a multi-agent team. Fetches Jira,
  GitHub, or GitLab via Rust forge-fetch → tmp JSON; prompts (or config-forces)
  model, agents, testers, approach (tdd|heads_down|plan), e2e, reviewers,
  evangelists; PO/designers/TDD/writeback with optional Figma/lore. Reuses
  scrutiny pack for post-impl review.
argument-hint: "[URL | issue-id | --inline description]"
---

# Forge

Ticket implement skill. **Scripts (Rust)** fetch ticket, write session plan,
context pack, brief. Agent judgment starts after artifacts exist.

Sibling of `/scrutiny` (same binary, `~/.scrutiny/config.toml`).

## Usage

- `/forge <Jira-URL|KEY-123>` — Jira (`acli`)
- `/forge <GitHub-issue-URL|#N|N>` — GitHub (`gh`)
- `/forge <GitLab-issue-URL>` — GitLab (`glab`)
- `/forge --inline <description>` — no remote ticket
- `/forge` — ask for URL or description
- Branch name with `PROJ-123` → Jira fetch when no arg

## Binary

Skill root = folder containing **this** `SKILL.md`.

```bash
SKILL_ROOT="<absolute-path-to-folder-containing-this-SKILL.md>"
SCRUTINY_BIN="$(bash "${SKILL_ROOT}/scripts/ensure-bin.sh")"
```

Config: `~/.scrutiny/config.toml` → `[forge]` (see shipped `config/default.toml`).

**Hard token rules**

- Agents read **ticket JSON + session JSON + brief markdown** (and pack after impl). No re-run `acli` / `gh` / `glab`.
- No dump full config into prompts — use `suggested_forge` / session fields.
- Caveman I/O. Partition workstreams. No full-repo fish when context paths exist.

---

## Agent workflow (mandatory order)

### 0. Parse args

- `--inline` / remaining text → inline description
- URL / key / number → remote
- Empty → ask user for URL or description; STOP until provided
- Detect client: Cursor → `cursor`, Claude Code → `claude`, Codex → `codex`; else config `default_client`

### 1. ensure-bin → forge-fetch

```bash
TICKET="$("$SCRUTINY_BIN" forge-fetch --cwd <repo-root> --client <client> --input "<arg>")"
# or: forge-fetch --inline --input "<desc>"
```

Show ticket path. Read `suggested_forge` from ticket JSON.

### 2. Confirm session (prompt only unset knobs)

| Prompt | Default | Skip when |
|--------|---------|-----------|
| Model | `suggested_forge.model` | `prompt_model` false |
| Approach `tdd` / `heads_down` / `plan` | `suggested_forge.approach` | `prompt_approach` false |
| Agents (developers) | `suggested_forge.agents` | `prompt_agents` false |
| Testers | `suggested_forge.testers` | `prompt_testers` false |
| E2E needed? | ask (yes/no) | `prompt_e2e` false → use `suggested_forge.e2e` (if null still ask once) |
| Reviewers (post-impl) | `suggested_forge.reviewers` | `prompt_reviewers` false |
| Evangelists (clean-code, post-impl) | `suggested_forge.evangelists` | `prompt_evangelists` false |

If `prompt_e2e` false and `e2e` is null in ticket suggested_forge, treat as **false**.

```bash
SESSION="$("$SCRUTINY_BIN" forge-plan-write \
  --ticket "$TICKET" --client <client> --model <model> \
  --approach <tdd|heads_down|plan> --e2e <true|false> \
  --agents <n> --testers <n> --reviewers <n> --evangelists <n> \
  --cwd <repo-root>)"
```

Show session path. Read flags: `enable_figma`, `enable_lore`, `enable_po`, `enable_ticket_writeback`, `skip_ai_review`, counts, approach.

### 3. Context + brief

```bash
CONTEXT="$("$SCRUTINY_BIN" forge-context --ticket "$TICKET" --cwd <repo-root>)"
BRIEF="$("$SCRUTINY_BIN" forge-brief --ticket "$TICKET" --session "$SESSION" --context "$CONTEXT")"
```

Read `markdown_path` from brief JSON. Pass brief path (+ ticket/session paths) to every later agent.

### 4. Approach branch

#### `plan`

1. Spawn **planner** agent (model from session). Input: brief + ticket paths only.
2. Planner writes implementation plan (files, steps, risks) to a temp markdown path; report path to user.
3. Ask user confirm / request changes (loop). **Exception:** never skip this confirm in `plan` mode.
4. Continue to Phase A with approved plan attached.

#### `tdd`

1. After PO/requirements (Phase A), spawn **test planner** → structured test-case list (unit + e2e if `session.e2e`).
2. Ask user: confirm / change / reject. Loop until confirm.
3. Testers write failing tests (red). Agents implement to green. See Phase C.

#### `heads_down`

- No human confirm after session plan.
- PO + test plan use **reviewer subagent** approve (max 2 revision rounds) instead of user.
- Implement until done; still run tests.

---

## Phase A — Gather + PO (full port)

### A1. Conventional commit prefix

From ticket type/title/labels (and branch prefix hint): `feat|fix|docs|refactor|perf|test|chore|…`. Ambiguous → ask (skip ask in `heads_down` — pick best).

### A2. Figma (`session.enable_figma`)

If false → skip silently.

If true:

1. Use `figma_urls` from ticket JSON + scan description/comments (already in ticket).
2. Jira only: associated designs via Atlassian GraphQL + basic auth (`ATLASSIAN_EMAIL` / `ATLASSIAN_TOKEN`) as in address-ticket (best-effort; continue on fail).
3. If URLs exist and `fcli` available: `fcli file info/inspect/export` → `/tmp/<id>-figma/`. Pass paths to PO/designers.
4. No `fcli` → show URLs; ask for manual assets (skip ask in `heads_down`).

### A3. Lore (`session.enable_lore`)

If false → skip silently.

If true and parent epic exists (Jira): maintain `lore/<EPIC-KEY>.md` (create/append ticket entry). Pass lore path to PO.

### A4. Split evaluation

If ticket too large for one PR (many areas / mixed types / sequential deps), propose split. User chooses. Jira: optional sub-tasks via `acli`. Proceed with first chunk only. Skip silently if not warranted. In `heads_down`: keep as one unless clearly unsafe — then note in report.

### A5. Product Owner (`session.enable_po`)

If false → synthesize minimal requirements from ticket title/description/brief; skip approve.

If true — spawn PO with: brief path, ticket path, attachments_dir, figma paths, lore, commit prefix, context related_paths / test_harness.

PO output structure (same substance as address-ticket):

```
## Ticket: <id>
**title** | Type | Priority | Commit prefix

### Understanding
### Requirements (numbered, testable)
### Acceptance Criteria (checklist)
### Design Needs (Yes/No + what)
### Implementation Plan
### Files to Modify / New Files
### Unit Tests / E2E Tests (honor session.e2e — if false, E2E = None)
### Risks / Open Questions
```

**Approve requirements**

- Not `heads_down`: show PO doc → Approve / Request changes / Reject.
- `heads_down`: spawn reviewer to challenge gaps; revise ≤2 rounds; then proceed.

### A6. Designers (conditional)

From PO Design Needs:

- No → skip
- Yes + Figma assets → 1 designer
- Yes + no Figma → 2 designers (research + propose) in parallel

Pass design guide to test planner + developers.

---

## Phase B — Test plan

Spawn test planner (not code). Input: approved requirements, design guide, context test_harness, `session.e2e`.

Output: unit (+ e2e if e2e) case list with file paths, AC coverage, edge cases.

Guidelines (keep):

- Every AC ≥1 test
- Bug → regression test
- No locale string asserts — use i18n keys
- No intermediate-state asserts that sibling epic tickets will break
- Follow project naming/layout from context

**Approve test plan**

- `tdd` / `plan` (interactive): user confirm/change/reject
- `heads_down`: reviewer subagent; ≤2 revisions

---

## Phase C — Implement

Team sizes from session: `testers`, `agents` (developers).

### C1. Red (testers)

If `testers` > 0 and approach is `tdd` or `plan` (after test approve): spawn tester agents — **tests only**, no production code. Partition unit vs e2e or by area. Verify fail for missing behavior (not syntax).

If `heads_down` and testers > 0: same red phase after auto-approved test plan.

If testers = 0: developers own tests while implementing.

### C2. Green (agents)

Spawn `agents` developers — production code to pass tests; **do not** weaken tests. Partition by independent files/areas. Resolve same-file conflicts. Lint/compile sanity.

### C3. Verify

Run unit (+ e2e if `session.e2e`). Fail → fix production (or genuine test defect). Max 2 fix loops then report remaining failures.

---

## Phase D — Post-impl review (reuse scrutiny)

If `session.skip_ai_review` → skip AI; optional note.

Else:

```bash
EVAL="$("$SCRUTINY_BIN" eval --cwd <repo-root> --client <client>)"
MAP="$("$SCRUTINY_BIN" map --cwd <repo-root> --eval "$EVAL")"
PACK="$("$SCRUTINY_BIN" pack --cwd <repo-root> --map "$MAP")"
SCAN="$("$SCRUTINY_BIN" scan --cwd <repo-root> --map "$MAP" --pack "$PACK" --eval "$EVAL")"
PLAN="$("$SCRUTINY_BIN" plan-write \
  --eval "$EVAL" --map "$MAP" --pack "$PACK" --scan "$SCAN" \
  --client <client> --model <session.model> \
  --security true --performance false --error-handling true \
  --reviewers <session.reviewers> --evangelists <session.evangelists>)"
```

Spawn reviewers / evangelists per plan (pack-only). Evangelists = clean-code / architecture. Merge scan + AI → caveman list. Interactive triage like `/scrutiny` unless `heads_down` (then auto-keep high-severity only, list rest).

**Model:** pass session model to Task `model=` (Claude: Anthropic aliases/ids only, not Cursor slugs).

---

## Phase E — Ticket writeback (`session.enable_ticket_writeback`)

If false → skip.

Compare original description vs approved requirements; propose improved description / comment.

- **Jira:** ask (or heads_down skip write unless clearly valuable comment). Update via `acli jira workitem edit --from-json` with ADF if user approves.
- **GitHub:** `gh issue comment` with summary + AC.
- **GitLab:** `glab issue note`.
- **Inline:** skip remote writeback.

---

## Phase F — Final report

```
## Forge complete: <id>
**<title>**
Source: … | Approach: … | Model: …

### Team
PO / designers / testers / agents / reviewers / evangelists counts

### Implementation
files +/− lines

### Tests
unit / e2e status

### Acceptance Criteria
[x] …

### Writeback
yes/no

### Next
PR / manual steps
```

---

## Edge cases

- Empty description → PO from title + comments
- Already done in codebase → stop and tell user before PO
- No test framework → skip red/green ceremony; agents implement + note
- Missing `acli`/`gh`/`glab` → forge-fetch errors; offer `--inline` paste
- Conflicting designer advice → ask user (`heads_down`: pick simpler)

---

## Token-saving checklist (mandatory)

1. Artifact-first paths only
2. Brief not full ticket dump in every spawn
3. Context related_paths before Grep sprawl
4. Post-review = scrutiny pack only
5. Config force knobs → zero prompts
6. `enable_figma` / `enable_lore` false when unused
7. Partition agent prompts by workstream
8. Caveman findings
9. `reviewers=evangelists=0` → skip AI review
