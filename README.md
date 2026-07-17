# Scrutiny

Agent skills for code review and ticket implementation, backed by a shared Rust CLI.

## Why scripts first

The goal is simple: **do as much work as possible outside the model.** Deterministic steps—diff analysis, packing, static scans, ticket fetch, line resolution, posting reviews—run as Rust commands that write small JSON artifacts. The agent only decides, confirms, and edits those files. That keeps prompts short, avoids re-exploring the repo, and spends tokens on judgment instead of plumbing.

## Skills

### `/scrutiny` (Probe)

Probes a local branch or a GitHub PR. Prefer **`scrutiny probe`** for script-orchestrated runs (detects Cursor/Claude/Codex CLIs, plan knobs, headless agents, triage, post). The `/scrutiny` skill remains for IDE agent sessions that chain discrete commands.

### `/forge`

Implements a ticket from Jira, GitHub, GitLab, or an inline description. Prefer **`scrutiny forge`** (script-orchestrated): fetch full ticket mirror under `.scrutiny/forge-<id>/`, export Figma via `fcli` when links exist, ask spawn/TDD/coverage/e2e/(playwright), optional TDD test-plan confirm loop, then single or team implement agent (writes `pr.json`, script commits + optional draft PR). Discrete `forge-fetch` / `forge-plan-write` / `forge-context` / `forge-brief` remain for IDE chaining. Post-impl review: `scrutiny probe`.

### `/parley`

Addresses unresolved GitHub PR review comments. Prefer **`scrutiny parley`**: GraphQL-fetch unresolved threads → ask members / verifiers / evangelists / spawn mode → isolated or team fix agents → verifier flag-pass (both modes: confirms fixes actually address comments) → optional evangelist verify (isolated) → host commit + push → script replies under each thread via `addPullRequestReviewThreadReply`. Set config root `headless = false` to run each agent in a visible terminal window (claude auto mode; tmux/zellij/macOS). Discrete `parley-fetch` / `parley-plan-write` / `parley-reply` for IDE chaining.

## Install (Homebrew)

Prebuilt `scrutiny` binary via the [`morphet81/homebrew-tools`](https://github.com/morphet81/homebrew-tools) tap (Apple Silicon macOS; Linux amd64/arm64):

```bash
brew tap morphet81/homebrew-tools
brew install scrutiny

# or one-shot without a prior tap:
brew install morphet81/homebrew-tools/scrutiny
```

Upgrade later: `brew update && brew upgrade scrutiny`.

Then install agent skills (binary already on PATH):

```bash
scrutiny skills-install -g -y --skill '*'
# or: npx skills add morphet81/scrutiny -g -y --skill '*'
```

## Install skills only

If the binary comes from elsewhere (`ensure-bin`, local build, etc.):

```bash
# via CLI (wraps npx skills add — uses local checkout when available)
scrutiny skills-install -g -y --skill '*'
./target/release/scrutiny skills-install --skill scrutiny --agent cursor

# or npx directly
npx skills add morphet81/scrutiny -g -y --skill '*'
npx skills add /path/to/scrutiny -g -y --skill '*' --agent cursor
```

Then `/scrutiny`, `/scrutiny <PR-URL>`, `/forge <ticket-URL>`, `/forge --inline <desc>`, `/parley`, `/parley <PR-URL>`.

### Prerequisites

- Prefer: [Homebrew](https://brew.sh/) + [`morphet81/homebrew-tools`](https://github.com/morphet81/homebrew-tools) for the CLI
- Or: network for GitHub Release binary / Rust toolchain for `cargo build --release`
- `git`
- Optional: `gh` (PR review + GitHub issues), `acli` (Jira), `glab` (GitLab), `fcli` (Figma)
- For `scrutiny probe`: headless agent CLI on PATH — `agent`/`cursor-agent`, `claude`, and/or `codex`
- `npx` for `skills-install`
- `SCRUTINY_GITHUB_REPO` overrides download/install repo (default `morphet81/scrutiny`)
- Binary fetch (when not using brew) uses GitHub Release **latest** by default (cache keyed by `bin/.scrutiny-version`; refreshes when tag changes). Set `SCRUTINY_VERSION=0.1.5` only to pin. `SCRUTINY_USE_LOCAL=1` forces local `cargo` build.

## Build (developers)

```bash
cargo build --release
./target/release/scrutiny eval --help
./target/release/scrutiny probe --help
./target/release/scrutiny skills-install --help
bash scripts/ensure-bin.sh
```

## Commands

### One-shot probe (preferred)

```bash
./target/release/scrutiny probe
./target/release/scrutiny probe --pr 42
./target/release/scrutiny probe --client claude --spawn-mode isolated
./target/release/scrutiny probe --from-json '{"client":"claude","model":"sonnet","security":true,"performance":false,"error_handling":true,"reviewers":1,"evangelists":0,"spawn_mode":"isolated"}' --yes
# resume triage/post from an existing AI review-report.json (skip eval/agents):
./target/release/scrutiny probe --from-report .scrutiny/42/report.json [--pr 42] [--scan .scrutiny/42/scan.json]
```

Flow: detect agent CLI → eval/map/pack/scan → plan-confirm → **team** lead (default) or **isolated** parallel headless agents → collate/dedupe (isolated) or lead report (team) → findings triage → `post-comments` → optional concern loop.

Artifacts live under **`<repo>/.scrutiny/<pr>/`** (or `.scrutiny/local/` without a PR): `eval.json`, `map.json`, `pack.json`, `scan.json`, `plan.json`, `findings.json`, `report.json`, …. Config stays in `~/.scrutiny/config.toml`. Each CLI run warns if `.scrutiny/` is missing from `.gitignore`.

`--from-report` skips analyze/agents: loads the AI report’s `findings`, inits a findings shell (from `--scan` if given, else empty), merges AI findings, then triage → post.

### One-shot forge (preferred)

```bash
./target/release/scrutiny forge PROJ-123
./target/release/scrutiny forge "https://…/browse/PROJ-123"
./target/release/scrutiny forge --inline --input "Add dark mode toggle"
./target/release/scrutiny forge --from-json '{"client":"claude","model":"sonnet","spawn_mode":"single","tdd":true,"e2e":true,"coverage_pct":100}' --yes --input KEY-1
```

### Forge bulk mode

Implement several tickets in one run — each on its own branch + worktree, run concurrently.

```bash
scrutiny forge bulk
scrutiny forge bulk --dry
scrutiny forge bulk --yes < tickets.txt
scrutiny forge bulk --concurrency 5
```

Interactive flow: a menu (**Paste ticket URL/key** / **Done**) collects tickets one at a time; **Done** ends collection (Done on the first prompt exits doing nothing). All tickets are validated (fetch + complexity sizing), then you pick **same settings for all** or **per-item settings** (same questions as single `forge`). Each item gets its own new branch + git worktree named `<type>-<projectkey>-<number>` (e.g. `feat-nero-8729`).

Items run concurrently, capped at `forge.bulk_concurrency` (default `3`; override with `--concurrency <N>`). Non-headless (claude + tmux/zellij/iTerm2/Terminal.app): each item opens its own terminal container (tmux session / zellij tab / iTerm2 window) named after the ticket key; agent panes are named by role (PO/developer/tester/reviewer/evangelist/tdd-plan/implement) and run in that item's worktree, where you validate its TDD plan. As each item finishes, the concluding step (confirm commit subject, PR title/body, create draft PR) is handled one item at a time on the main terminal.

Requires a git repository (per-item worktrees). tmux is the most reliable multiplexer; Terminal.app grouping is best-effort.

Flags:

- `--yes` — headless/non-interactive: reads newline-separated ticket keys/URLs from stdin, auto-answers from complexity suggestions, auto-commits, auto-creates draft PRs, no prompts.
- `--dry` — runs the whole flow but spawns **no** agents and creates **no** real PR. Branches + worktrees are still created; non-headless panes are created (showing what would run) but never auto-closed; at the end you're offered to delete the created branches + worktrees.
- `--concurrency <N>` — override the concurrency cap.

### One-shot parley (preferred)

```bash
./target/release/scrutiny parley
./target/release/scrutiny parley --pr 42
./target/release/scrutiny parley --from-json '{"client":"claude","model":"sonnet","members":2,"evangelists":1,"spawn_mode":"isolated"}' --yes
```

Flow: GraphQL unresolved `reviewThreads` → `.scrutiny/<pr>/parley-comments.json` → knobs (members ≤ comment count, verifiers, evangelists, isolated|team) → fix agents write `parley-fixes.json` → verifier flag-pass (both modes; writes `verified`/`verification`, flips bogus `addressed`) → optional evangelist verify (isolated only) → host `git commit` + `git push` → script `parley-reply` under each thread id. Agents must not commit/push/gh-reply. Config root `headless = false` opens each agent in a visible auto-mode window (claude; tmux/zellij/macOS), else headless.

Flow: require source CLI (`acli`/`gh`/`glab`) with install links → ticket mirror under `.scrutiny/forge-<id>/` (attachments, full fields) → if Figma URLs require `fcli` and export screenshots+XML → ask spawn (**single** default|team), playwright (skipped if no `playwright-cli`), TDD, coverage%, e2e → optional TDD test-plan agent + confirm/comment → implement agent (prompt encodes choices; writes `.scrutiny/forge-<id>/pr.json` with PR title/body + commit message; cleans non-implementation junk; does **not** commit) → script commits from `pr.json` → TTY asks to create a **draft PR** (base branch defaults to calculated base; skipped with `--yes` / non-TTY).

Install links when missing: [acli](https://developer.atlassian.com/cloud/acli/guides/install-acli/), [fcli](https://github.com/morphet81/figma-cli).

Claude: log in once (`claude` then `/login`) so OAuth works. `scrutiny probe` does **not** pass `--bare` unless `ANTHROPIC_API_KEY` is set or `SCRUTINY_CLAUDE_BARE=1`. Force OAuth even with a key: `SCRUTINY_CLAUDE_NO_BARE=1`.

Config (`~/.scrutiny/config.toml`):

```toml
# force_client = "claude"    # skip client prompt (default_client is already claude)
# force_spawn_mode = "isolated"  # or "team"
```

### Step-by-step probe pipeline

```bash
./target/release/scrutiny eval
./target/release/scrutiny eval --base main --head abcdef0 --client claude --pr 42
./target/release/scrutiny map --eval .scrutiny/42/eval.json
./target/release/scrutiny pack --map .scrutiny/42/map.json
./target/release/scrutiny scan --map .scrutiny/42/map.json --pack .scrutiny/42/pack.json --eval .scrutiny/42/eval.json
# interactive: knobs in one session (or --from-json for CI)
./target/release/scrutiny plan-confirm --eval .scrutiny/42/eval.json
./target/release/scrutiny plan-write --eval .scrutiny/42/eval.json --map .scrutiny/42/map.json \
  --pack .scrutiny/42/pack.json --scan .scrutiny/42/scan.json \
  --answers .scrutiny/42/plan-answers.json
# after spawning reviewers/evangelists:
./target/release/scrutiny pack-partition --pack .scrutiny/42/pack.json --reviewers 2
./target/release/scrutiny probe-session-write --plan .scrutiny/42/plan.json --pack .scrutiny/42/pack.json \
  --from-json '[{"role":"reviewer","index":1,"paths":["a.rs"],"findings_count":2}]'
./target/release/scrutiny findings-init --scan .scrutiny/42/scan.json --eval .scrutiny/42/eval.json \
  --pack .scrutiny/42/pack.json --plan .scrutiny/42/plan.json --pr 42
./target/release/scrutiny findings-triage --findings .scrutiny/42/findings.json
./target/release/scrutiny findings-resolve --findings .scrutiny/42/findings.json
./target/release/scrutiny findings-validate --findings .scrutiny/42/findings.json
./target/release/scrutiny post-comments --findings .scrutiny/42/findings.json
```

### eval complexity

`eval` scores XS…XL from diff size/scatter/risk/layers. **Not scored:** docs (`.md`, `docs/`, … — still listed for map). **LOC:** comment-only `+/-` lines stripped (e.g. `//`, `/* */`, `#`, `--`, `<!-- -->`). Noise globs still fully excluded.

### plan-confirm / plan-write

`plan-confirm` asks (TTY ↑/↓ menus + confirms): model, security, performance, error-handling, reviewers, evangelists, **spawn_mode** (`team` default | `isolated`) — defaults from eval `suggested_plan`. Prints answers JSON path. `plan-write --answers` applies caps: `max_reviewers` when pack is small (`pack_chars < 4000` → 1), evangelists only with architecture risk / tier L+, `skip_ai` when XS+docs or no agents/specialists.

### Spawn modes

- **isolated (default):** script runs reviewers + evangelists + analysis specialists in parallel with shared `build_isolated_prompt` templates; script collates and dedupes. Prefer this for token cost.
- **team:** one lead headless agent gets `build_team_lead_prompt`, which **embeds the same isolated role briefs verbatim**. Lead pastes those templates when spawning members, waits for all JSON returns, keeps higher severity on conflicts, then returns one findings JSON. Higher token cost (lead re-bills member transcripts).

Print a role or lead prompt for skill/debug:

```bash
./target/release/scrutiny agent-prompt --role reviewer --pack .scrutiny/42/pack.json [--plan .scrutiny/42/plan.json] [--paths a.rs,b.rs]
./target/release/scrutiny agent-prompt --role lead --pack .scrutiny/42/pack.json --plan .scrutiny/42/plan.json
```

### Review session

`pack-partition` splits pack slice paths across N reviewers (round-robin). `probe-session-write` records spawned agents and **fails** if counts do not match the plan (team mode expects one `lead`).

### Findings / post-comments

After triage, findings live in a structured JSON file (`include`, `chosen_option`, `comment_body`, `anchor`, `review.event`). Severities: `critical` | `warning` | `suggestion`.

`findings-triage` (and `scrutiny probe`) shows each finding critical-first. On a TTY: **↑/↓ menu** — Post / Ignore / Ask a question… (or fix option A/B…). Ask is a separate menu item, then a follow-up question prompt — never free-text on the same line as P/I. Agent revises that finding only; menu reappears. Non-TTY: `P` / `I` / option letter, or `ask <question>`.

On a TTY, severity/title use ANSI colors (`NO_COLOR` or non-TTY disables). Each finding shows a short code snippet from `git show <head>:<path>` when a path exists.

`post-comments` requires a GitHub PR. It prompts for `COMMENT` / `REQUEST_CHANGES` / `APPROVE` (or `--event`), then creates one PR review with **inline comments** (one per included finding with a diff line). Bodies end with `[AI Agent]`.

Comment placement:

- **Line** — path + line on the **PR/pack unified diff** → GitHub review comment (`path`/`line`/`side`)
- **File** — path but no commentable line (missing line, or line not on the PR patch) → `"subject_type": "file"` (post still succeeds; demotes automatically)
- **Global** — no path → `### Global notes` in the review body

Scan seeds are **change-scoped** (added lines / change map / large added surface in the pack diff). Agents should still cite PR-diff lines; if a Post’d finding has a non-commentable line, scrutiny posts a file comment instead of failing the run. Failed GitHub review creates do **not** silently dump comments into the review body.

If the authenticated user already has a **PENDING** review on that PR, the script asks:

1. Add these comments to the pending review (GraphQL append — existing draft line anchors kept), then submit it  
2. Close the pending review (choose event), then create a **new** review with the findings  

Agents must not call `gh` review create/dismiss/delete — only `post-comments`.

Line anchors are verified with `git show <head_oid>:<path>` and PR file patches.

### Forge pipeline

```bash
./target/release/scrutiny forge-fetch --input "https://github.com/o/r/issues/1"
./target/release/scrutiny forge-fetch --inline --input "Add dark mode"
./target/release/scrutiny forge-plan-write --ticket … \
  --client cursor --model composer-2-fast --approach tdd \
  --e2e false --agents 2 --testers 1 --reviewers 1 --evangelists 0
./target/release/scrutiny forge-context --ticket …
./target/release/scrutiny forge-brief --ticket … --session … --context …
```

Each command prints **one** temp JSON path on stdout (`forge-brief` also writes a `.md` path inside the JSON).

## Config

First run copies `config/default.toml` → `~/.scrutiny/config.toml`. Every key has a default
(`#[serde(default)]`), so a partial file is valid — set only what you override.

### Project-local `scrutiny.toml`

Drop a `scrutiny.toml` in your project to override the global config **per item**. Scrutiny
walks up from the working directory to the repo root and uses the first `scrutiny.toml` it
finds. Merge is deep and per-key: any key set locally wins, everything else falls back to
`~/.scrutiny/config.toml`. Tables merge key-by-key; scalars and arrays are replaced wholesale
(a local array replaces the global one, it does not append).

```toml
# <repo>/scrutiny.toml — override just what this repo needs
default_client = "codex"

[models.claude]
m = "sonnet"        # only tier `m` changes; other tiers stay global

[git]
base_candidates = ["develop", "main"]   # replaces the global list
```

### Full key reference

**Top-level**

| Key | Default | Meaning |
|-----|---------|---------|
| `default_client` | `"claude"` | AI client: `claude` \| `cursor` \| `codex` |
| `headless` | `true` | Capture agent stdout. `false` → open each agent in a visible terminal window (claude only; tmux/zellij/macOS Terminal/iTerm2). Applies to probe, parley, forge. Permission mode is model-aware: models that support `--permission-mode auto` (opus/sonnet 4.6+, fable) run unattended; older ones (haiku, sonnet/opus 4.5, claude-3) fall back to manual approval in the pane (non-headless) or `--dangerously-skip-permissions` (headless), with a one-time warning |
| `force_client` | _(unset)_ | Skip the client prompt for `scrutiny probe` |
| `force_spawn_mode` | _(unset)_ | Skip spawn-mode prompt: `isolated` \| `team`. Unset → `isolated` |
| `editor` | _(unset)_ | Editor for PR descriptions; falls back to `$VISUAL` → `$EDITOR` → `vi`. Supports args (`"code --wait"`) |

**`[models.<client>]`** — model per tier for `claude` / `cursor` / `codex`. Keys `xs`,`s`,`m`,`l`,`xl`. Claude uses aliases (`haiku`/`sonnet`/`opus`) or pinned Anthropic ids — not Cursor slugs.

**`[review]`** — per-tier toggles `security_by_tier`, `performance_by_tier`, `error_handling_by_tier` (each `XS`/`S`/`M`/`L`/`XL` bool). Security/performance/error-handling default off for XS/S, on for M/L/XL.

**`[review.signals]`** — content-signal gating: `ignore_content_signals` (`false`), plus glob/regex lists `security_path_globs`, `security_diff_patterns`, `performance_path_globs`, `performance_diff_patterns`, `performance_css_path_globs`, `performance_css_patterns`, `error_handling_diff_patterns`.

**`[agents]`** — `max_agents_total` (`4`), `max_reviewers` (`2`), `max_evangelists` (`1`); per-tier counts `[agents.reviewers_by_tier]` and `[agents.evangelists_by_tier]` (`XS`..`XL`).

**`[git]`** — `base_candidates` (`["main","master","develop"]`), `exclude_globs` (lockfiles, `node_modules`, `dist`, generated, …).

**`[pack]`** — review-pack budgets: `max_chars` (`48000`), `doc_digest_lines` (`40`), `symbol_context_lines` (`3`), `min_file_chars` (`1200`), weights `source_weight` (`4`)/`test_weight` (`2`)/`doc_weight` (`1`), xref knobs `enable_xref` (`true`), `xref_max_symbols` (`40`), `xref_max_files_scanned` (`300`), `xref_char_budget` (`6000`), `xref_body_lines` (`40`), `annex_char_budget` (`12000`).

**`[pack.explore]`** — bounded agent exploration: `enable` (`true`), `max_extra_reads` (`6`), `max_extra_chars` (`24000`), `prefer_read_over_bash` (`true`), `allow_repo_grep` (`false`), `require_pack_path_hint` (`true`).

**`[scan]` / `[scan.i18n]`** — deterministic scan: `enable` (`true`), `commands` (extra lint hooks); i18n `enable` (`true`), `reference_locale` (`"en"`), `path_globs`, `check_placeholders` (`true`), `check_empty_values` (`true`), `full_catalog` (`false`).

**`[forge]`** — force approach / e2e / agent counts (omit = prompt): `approach`, `e2e`, `agents`, `testers`, `reviewers`, `evangelists`, `model`; toggles `enable_figma`/`enable_lore`/`enable_po`/`enable_ticket_writeback`/`enable_branch` (all `true`); defaults `default_approach` (`"tdd"`), `default_agents` (`2`), `default_testers` (`1`), `default_reviewers` (`1`), `default_evangelists` (`0`); verify gate `verify_commands`, `verify_max_loops` (`2`), `verify_coverage` (`true`); `branch_headless` (`"auto"`); `bulk_concurrency` (`3`); `pr_description_prompt` _(unset)_ — when set, a dedicated headless agent writes the PR body from this prompt + the diff (overrides the implement agent's `pr_body`).

**`[forge.complexity]`** — keyword lists, story-point field names, and tier thresholds that drive automatic model selection.

**`[parley]`** — `default_members` (`1`), `default_evangelists` (`1`), `default_verifiers` (`1`), `push_fix_max_loops` (`2`).

**`[prompts]`** — custom prompt injection (see below).

### Custom prompts

Inject your own text into scrutiny's spawned-agent prompts without editing the binary.

- `[prompts].global` — prepended to **every** spawned agent (probe, parley, forge).
- `[prompts.agents].<role>` — prepended for one role only.

Order: **`global` → `<role>` → scrutiny's own prompt**. Both trimmed; empty entries skipped.

Role key = the agent's label prefix with `-` → `_`. Unknown keys are silently ignored.

| Surface | Role keys |
|---------|-----------|
| probe | `reviewer`, `evangelist`, `security`, `performance`, `error_handling`, `lead` |
| parley | `parley_member`, `parley_lead`, `parley_verifier`, `parley_evangelist`, `parley_push_fix` |
| forge | `forge_test_plan`, `forge_test_plan_revise`, `forge_implement`, `forge_po_team`, `forge_verify_fix` |

Caveat: in **team** mode the lead spawns teammates itself — injection reaches only the prompt
scrutiny builds (the lead's), not sub-agents the lead spawns.

```toml
[prompts]
global = "Repo convention: cite file:line. Never edit generated code."

[prompts.agents]
reviewer = "Prioritise null-safety and async race conditions."
security = "We handle PCI data — flag any logging of card fields."
forge_implement = "Match the existing Result/anyhow error style."
```

### Forge model selection

`scrutiny forge` estimates ticket complexity **before prompting for the model**. Signals (all deterministic, no AI call):

| Signal | Source | Notes |
|--------|--------|-------|
| AC count | Checkboxes / numbered list under AC heading / BDD Scenarios | Bucket → points |
| Description size | Word count | Bucket → points |
| Breadth keywords | title + description (refactor, migrate, overhaul…) | +8 pts/hit, capped at 2 |
| Integration keywords | api, database, webhook, migration… | +6 pts/hit, capped at 2 |
| Risk keywords | auth, security, payment, pii… | +10 pts/hit, capped at 2 |
| Trivial keywords | typo, wording, bump, minor… | −8 pts/hit, capped at 2 |
| Story points | Jira custom field (`story_point_fields`) | Dominant: 1-2→S, 3-5→M, 6-8→L, 9+→XL |
| Issue type | Jira `issuetype.name` | Epic +15, Story +8, Bug −3, Subtask −8 |
| Labels | config `bump_labels` / `lower_labels` | ±6 pts, max 1 hit each |
| Figma URLs | ticket | +5 pts (UI work) |
| Comments | ticket | 0/2/5/8 pts |

Score 0–100 → tier XS/S/M/L/XL → `[models.<client>]` lookup → default selection in the model prompt (user can still change it). Override with `[forge] model = "sonnet"` to pin globally.

Example force (no prompts):

```toml
[forge]
approach = "tdd"
e2e = false
agents = 2
testers = 1
reviewers = 1
evangelists = 0
model = "sonnet"      # pin model, skip complexity prompt
enable_figma = false
enable_lore = false
# pr_description_prompt = "Summarize for reviewers: what changed, why, risk, test notes."

[forge.complexity]
# Extend risk keywords for your domain
risk_keywords = ["auth", "security", "payment", "pii", "credential", "oauth", "token", "billing"]
# Your Jira story-point custom field
story_point_fields = ["customfield_10016"]
```

## Token-saving habits

Same idea across both skills: artifact paths in, not raw CLI dumps; pack/brief instead of full-file fishing; config force knobs to skip prompts; turn off Figma/lore when unused; set reviewers/evangelists to `0` when you want static-only.

Review specifics:
- Prefer **isolated** spawn (default) over team.
- Locale/i18n files are **not** AI-reviewed — `scan.i18n` flags missing keys across languages.
- Security/performance defaults follow **content signals** (network/auth vs hooks/domain), not tier alone.
- Agents use graduated exploration (pack → allowlisted fetch → capped extra Reads). Avoid whole-repo `rg`.
- Tune `~/.scrutiny/config.toml`: `[review.signals]`, `[pack.explore]`, `[agents].max_agents_total`.

## Releases

**Not on crates.io** — both crates set `publish = false`; `release.toml` keeps `cargo release` from publishing.

Bump + tag (no registry):

```bash
cargo release patch --execute   # or minor / major
```

Tag `v*` runs `.github/workflows/release.yml` and uploads platform binaries; `ensure-bin.sh` downloads the host asset when present.

Released targets: `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`. Intel Mac (`x86_64-apple-darwin`) is not shipped — `ensure-bin` builds with cargo instead.

## Layout

```
skills/scrutiny/SKILL.md   # /scrutiny
skills/forge/SKILL.md      # /forge
scripts/ensure-bin.sh      # shared (also copied under each skill)
config/default.toml
crates/scrutiny-cli/
crates/scrutiny-core/
```
