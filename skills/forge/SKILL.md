---
name: forge
description: >-
  Implement a ticket or description. Prefer `scrutiny forge` (fetch mirror,
  fcli, knobs, TDD plan confirm, single|team implement). Or chain forge-fetch /
  plan-write / context / brief. Reuses scrutiny probe for post-impl.
argument-hint: "[URL | issue-id | --inline description]"
---

# Forge

**Preferred (script-orchestrated):**

```bash
SKILL_ROOT="<absolute-path-to-folder-containing-this-SKILL.md>"
SCRUTINY_BIN="$(bash "${SKILL_ROOT}/scripts/ensure-bin.sh")"
"$SCRUTINY_BIN" forge [--cwd <repo-root>] [--client <client>] <URL|KEY|#N>
# or: forge --inline --input "<desc>"
```

That requires the source CLI (`acli` / `gh` / `glab`) with install URLs on miss,
mirrors ticket under `.scrutiny/forge-<id>/`, exports Figma via `fcli` when links
exist, asks spawn (default **single**)|team, playwright (skip if missing), TDD,
coverage, e2e → **scaffolding** (guess+confirm prefix, optional branch/worktree)
→ optional test-plan confirm → implement agent → verify gate → ship step.

**Scaffolding (host-owned, before implement):** host guesses a conventional
prefix from ticket type/labels/title → confirm via Select (guess first). If
`enable_branch`, detect git state and offer *create branch* / *+worktree* /
*use current* (default depends on whether you're on a base branch); worktree
switches the working dir for implement+commit. Non-TTY follows
`branch_headless` (`auto` = create only when on a base branch; `never` = current).

Implement agent must write `.scrutiny/forge-<id>/pr.json`
(`pr_title`, `pr_body` citing the ticket URL only, `commit_subject` starting with
the chosen prefix, `commit_body`), delete non-implementation junk (e.g. playwright
temp media), and must **not** create branches, commit, push, or open a PR. After
the agent exits, `scrutiny forge` confirms the commit subject (Input, default =
AI value or guess), commits, then on a TTY asks whether to create a **draft PR**
(PR-title Input + base branch prompt). `--yes` / non-TTY skips prompts and uses
the defaults.

Sibling of `/scrutiny` and `/parley` (same binary, `~/.scrutiny/config.toml`).

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

- Agents read **ticket JSON + session JSON + brief markdown** (and pack after impl). No re-run `acli` / `gh` / `glab` / `fcli`.
- No dump full config into prompts — use `suggested_forge` / session fields.
- Caveman I/O. Partition workstreams. No full-repo fish when context paths exist.

---

## Agent workflow (IDE chaining — only if not using `scrutiny forge`)

### 0. Parse args

- `--inline` / remaining text → inline description
- URL / key / number → remote
- Empty → ask user for URL or description; STOP until provided
- Detect client: Cursor → `cursor`, Claude Code → `claude`, Codex → `codex`; else config `default_client`

Prefer jumping to **`$SCRUTINY_BIN forge`** instead of step 1+.

### 1. ensure-bin → forge-fetch

```bash
TICKET="$("$SCRUTINY_BIN" forge-fetch --cwd <repo-root> --client <client> --input "<arg>")"
# or: forge-fetch --inline --input "<desc>"
```

Show ticket path. Read `suggested_forge` from ticket JSON.

### 2. Confirm session (prompt only unset knobs)

| Prompt | Default | Skip when |
|--------|---------|-----------|
| Model | `suggested_forge.model` (derived from ticket complexity tier — AC count, keywords, story points → XS/S/M/L/XL → `[models.<client>]` lookup) | `prompt_model` false |
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

`scrutiny forge` guesses the prefix (`feat|fix|docs|refactor|perf|test|chore`)
from ticket type/labels/title and confirms it with the user before implement.
The agent must **honor the provided prefix** (in `commit_subject`), not re-derive
it. IDE chain (no `scrutiny forge`): derive as before, ask if ambiguous.

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
- **Case titles:** affirmative; start with a bare verb (`renders…`, `returns…`, `opens…`, `does not…`); no `should` / `should not`; no `TC-*` / numbered / ticket-id prefixes; nested `describe` = SUT/area only. Prefer matching nearby `it()`/`test()` style when present.

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

### C3. Verify — host-owned gate (not the agent)

`scrutiny forge` runs the verify gate itself after implement; the agent no longer
decides "done". Do NOT re-implement this in the IDE chain.

- Commands: `[forge].verify_commands` if set, else auto-derived from sniffed
  harness + project files: tests (`vitest`→`npx vitest run --reporter=json`,
  `cargo-test`→`cargo test`, …; e2e only when `session.e2e`) **plus lint/build**
  (`cargo clippy`; `<pm> run lint|typecheck|build` from package.json; `tsc --noEmit`).
- Coverage: gated when `[forge].verify_coverage` and measurable (per-framework
  json summary); unmeasurable → warn, never block.
- Loop: on red, host spawns `forge-verify-fix` with a **surgical** payload —
  failing test `file:line` + message, uncovered line ranges — up to
  `verify_max_loops` (default 2).
- Commit gate: green → commit; still red + TTY → ask "commit anyway?"; still red +
  non-TTY → no commit, exit non-zero.

---

## Phase D — Post-impl probe (reuse `scrutiny probe`)

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
