# Scrutiny

Agent skills for code review and ticket implementation, backed by a shared Rust CLI.

## Why scripts first

The goal is simple: **do as much work as possible outside the model.** Deterministic steps—diff analysis, packing, static scans, ticket fetch, line resolution, posting reviews—run as Rust commands that write small JSON artifacts. The agent only decides, confirms, and edits those files. That keeps prompts short, avoids re-exploring the repo, and spends tokens on judgment instead of plumbing.

## Skills

### `/scrutiny`

Reviews a local branch or a GitHub PR. Prefer **`scrutiny review`** for script-orchestrated runs (detects Cursor/Claude/Codex CLIs, plan knobs, headless agents, triage, post). The `/scrutiny` skill remains for IDE agent sessions that chain discrete commands.

### `/forge`

Implements a ticket from Jira, GitHub, GitLab, or an inline description. Scripts fetch and normalize the ticket, write a session plan (approach, team sizes, e2e, post-review counts—forceable in config), and produce a compact context pack plus caveman brief so implementers never re-hit the ticket CLIs. The agent then runs plan, TDD, or heads-down modes with PO/testers/developers as configured, and can reuse the scrutiny pack pipeline for post-implementation review.

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

Then `/scrutiny`, `/scrutiny <PR-URL>`, `/forge <ticket-URL>`, `/forge --inline <desc>`.

### Prerequisites

- Prefer: [Homebrew](https://brew.sh/) + [`morphet81/homebrew-tools`](https://github.com/morphet81/homebrew-tools) for the CLI
- Or: network for GitHub Release binary / Rust toolchain for `cargo build --release`
- `git`
- Optional: `gh` (PR review + GitHub issues), `acli` (Jira), `glab` (GitLab), `fcli` (Figma)
- For `scrutiny review`: headless agent CLI on PATH — `agent`/`cursor-agent`, `claude`, and/or `codex`
- `npx` for `skills-install`
- `SCRUTINY_GITHUB_REPO` overrides download/install repo (default `morphet81/scrutiny`)
- Binary fetch (when not using brew) uses GitHub Release **latest** by default (cache keyed by `bin/.scrutiny-version`; refreshes when tag changes). Set `SCRUTINY_VERSION=0.1.5` only to pin. `SCRUTINY_USE_LOCAL=1` forces local `cargo` build.

## Build (developers)

```bash
cargo build --release
./target/release/scrutiny eval --help
./target/release/scrutiny review --help
./target/release/scrutiny skills-install --help
bash scripts/ensure-bin.sh
```

## Commands

### One-shot review (preferred)

```bash
./target/release/scrutiny review
./target/release/scrutiny review --pr 42
./target/release/scrutiny review --client claude --spawn-mode isolated
./target/release/scrutiny review --from-json '{"client":"claude","model":"sonnet","security":true,"performance":false,"error_handling":true,"reviewers":1,"evangelists":0,"spawn_mode":"isolated"}' --yes
# resume triage/post from an existing AI review-report.json (skip eval/agents):
./target/release/scrutiny review --from-report /tmp/…-report.json [--pr 42] [--scan /tmp/…-scan.json]
```

Flow: detect agent CLI → eval/map/pack/scan → plan-confirm → **team** lead (default) or **isolated** parallel headless agents → collate/dedupe (isolated) or lead report (team) → findings triage → `post-comments` → optional concern loop.

`--from-report` skips analyze/agents: loads the AI report’s `findings`, inits a findings shell (from `--scan` if given, else empty), merges AI findings, then triage → post.

Claude: log in once (`claude` then `/login`) so OAuth works. `scrutiny review` does **not** pass `--bare` unless `ANTHROPIC_API_KEY` is set or `SCRUTINY_CLAUDE_BARE=1`. Force OAuth even with a key: `SCRUTINY_CLAUDE_NO_BARE=1`.

Config (`~/.scrutiny/config.toml`):

```toml
# force_client = "claude"    # skip client prompt (default_client is already claude)
# force_spawn_mode = "team"  # or "isolated"
```

### Step-by-step review pipeline

```bash
./target/release/scrutiny eval
./target/release/scrutiny eval --base main --head abcdef0 --client claude
./target/release/scrutiny map --eval /tmp/scrutiny-…-eval.json
./target/release/scrutiny pack --map /tmp/scrutiny-…-map.json
./target/release/scrutiny scan --map … --pack … --eval …
# interactive: knobs in one session (or --from-json for CI)
./target/release/scrutiny plan-confirm --eval …
./target/release/scrutiny plan-write --eval … --map … --pack … --scan … \
  --answers /tmp/…-plan-answers.json
# after spawning reviewers/evangelists:
./target/release/scrutiny pack-partition --pack … --reviewers 2
./target/release/scrutiny review-session-write --plan … --pack … \
  --from-json '[{"role":"reviewer","index":1,"paths":["a.rs"],"findings_count":2}]'
./target/release/scrutiny findings-init --scan … --eval … --pack … --plan … [--pr 42]
./target/release/scrutiny findings-triage --findings …
./target/release/scrutiny findings-resolve --findings …
./target/release/scrutiny findings-validate --findings …
./target/release/scrutiny post-comments --findings …
```

### eval complexity

`eval` scores XS…XL from diff size/scatter/risk/layers. **Not scored:** docs (`.md`, `docs/`, … — still listed for map). **LOC:** comment-only `+/-` lines stripped (e.g. `//`, `/* */`, `#`, `--`, `<!-- -->`). Noise globs still fully excluded.

### plan-confirm / plan-write

`plan-confirm` asks (TTY ↑/↓ menus + confirms): model, security, performance, error-handling, reviewers, evangelists, **spawn_mode** (`team` default | `isolated`) — defaults from eval `suggested_plan`. Prints answers JSON path. `plan-write --answers` applies caps: `max_reviewers` when pack is small (`pack_chars < 4000` → 1), evangelists only with architecture risk / tier L+, `skip_ai` when XS+docs or no agents/specialists.

### Spawn modes

- **team (default):** one lead headless agent gets `build_team_lead_prompt`, which **embeds the same isolated role briefs verbatim**. Lead must paste those templates when spawning members (no inventing prompts), wait for all JSON returns, keep higher severity on conflicts, then return one findings JSON.
- **isolated:** script runs reviewers + evangelists + analysis specialists in parallel with shared `build_isolated_prompt` templates; script collates and dedupes.

Print a role or lead prompt for skill/debug:

```bash
./target/release/scrutiny agent-prompt --role reviewer --pack /tmp/…-pack.json [--plan /tmp/…-plan.json] [--paths a.rs,b.rs]
./target/release/scrutiny agent-prompt --role lead --pack /tmp/…-pack.json --plan /tmp/…-plan.json
```

### Review session

`pack-partition` splits pack slice paths across N reviewers (round-robin). `review-session-write` records spawned agents and **fails** if counts do not match the plan (team mode expects one `lead`).

### Findings / post-comments

After triage, findings live in a structured JSON file (`include`, `chosen_option`, `comment_body`, `anchor`, `review.event`). Severities: `critical` | `warning` | `suggestion`.

`findings-triage` (and `scrutiny review`) shows each finding critical-first. On a TTY: **↑/↓ menu** — Post / Ignore / Ask a question… (or fix option A/B…). Ask is a separate menu item, then a follow-up question prompt — never free-text on the same line as P/I. Agent revises that finding only; menu reappears. Non-TTY: `P` / `I` / option letter, or `ask <question>`.

On a TTY, severity/title use ANSI colors (`NO_COLOR` or non-TTY disables). Each finding shows a short code snippet from `git show <head>:<path>` when a path exists.

`post-comments` requires a GitHub PR. It prompts for `COMMENT` / `REQUEST_CHANGES` / `APPROVE` (or `--event`), then creates one PR review with **inline comments** (one per included finding with a diff line). Bodies end with `[AI Agent]`.

Comment placement:

- **Line** — path + line on the **PR/pack unified diff** → GitHub review comment (`path`/`line`/`side`)
- **File** — path but no commentable line (missing line, or line not on the PR patch) → `"subject_type": "file"` (post still succeeds; demotes automatically)
- **Global** — no path → `### Global notes` in the review body

Scan seeds are **change-scoped** (added lines / change map / large added surface in the pack diff). Agents should still cite PR-diff lines; if a Post’d finding has a non-commentable line, scrutiny posts a file comment instead of failing the run. Failed GitHub review creates do **not** silently dump comments into the review body.

If the authenticated user already has a **PENDING** review on that PR, the script asks:

1. Add these comments to the pending review, then submit it  
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

First run copies `config/default.toml` → `~/.scrutiny/config.toml`.

- `[models.claude]` — Claude Code aliases (`haiku`/`sonnet`/`opus`) or pinned ids. Not Cursor slugs.
- `[pack]` / `[scan]` — review pack budget + optional lint hooks
- `[forge]` — force approach / e2e / agent counts (omit = prompt); `enable_figma`, `enable_lore`, `enable_po`, `enable_ticket_writeback`

Example force (no prompts):

```toml
[forge]
approach = "tdd"
e2e = false
agents = 2
testers = 1
reviewers = 1
evangelists = 0
model = "sonnet"
enable_figma = false
enable_lore = false
```

## Token-saving habits

Same idea across both skills: artifact paths in, not raw CLI dumps; pack/brief instead of full-file fishing; config force knobs to skip prompts; turn off Figma/lore when unused; set reviewers/evangelists to `0` when you want static-only.

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
