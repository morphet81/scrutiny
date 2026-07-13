# Scrutiny

Agent skills for code review and ticket implementation, backed by a shared Rust CLI.

## Why scripts first

The goal is simple: **do as much work as possible outside the model.** Deterministic steps—diff analysis, packing, static scans, ticket fetch, line resolution, posting reviews—run as Rust commands that write small JSON artifacts. The agent only decides, confirms, and edits those files. That keeps prompts short, avoids re-exploring the repo, and spends tokens on judgment instead of plumbing.

## Skills

### `/scrutiny`

Reviews a local branch or a GitHub PR. Scripts score complexity, build a change map, pack only the relevant diffs and symbol slices, run a zero-token static scan, then `plan-confirm` collects all plan knobs in one stdin session (model, analyses, reviewers, evangelists). Optional AI reviewers read partitioned pack paths; a review-session artifact validates spawn counts. Findings are tracked in a structured JSON file you triage; `post-comments` posts them (handles pending reviews) with precise line anchors and an `[AI Agent]` tag.

### `/forge`

Implements a ticket from Jira, GitHub, GitLab, or an inline description. Scripts fetch and normalize the ticket, write a session plan (approach, team sizes, e2e, post-review counts—forceable in config), and produce a compact context pack plus caveman brief so implementers never re-hit the ticket CLIs. The agent then runs plan, TDD, or heads-down modes with PO/testers/developers as configured, and can reuse the scrutiny pack pipeline for post-implementation review.

## Install with `npx skills`

[`npx skills`](https://github.com/vercel-labs/skills) copies skill folders. It does **not** compile Rust; each skill’s `scripts/ensure-bin.sh` downloads a GitHub Release binary or builds from source on first use.

```bash
# both skills
npx skills add morphet81/scrutiny -g -y --skill '*'

# one skill
npx skills add morphet81/scrutiny@scrutiny -g -y
npx skills add morphet81/scrutiny@forge -g -y

# local checkout
npx skills add /path/to/scrutiny -g -y --skill '*' --agent cursor
```

Then `/scrutiny`, `/scrutiny <PR-URL>`, `/forge <ticket-URL>`, `/forge --inline <desc>`.

### Prerequisites

- `git`
- Network for release binary **or** Rust toolchain for `cargo build --release`
- Optional: `gh` (PR review + GitHub issues), `acli` (Jira), `glab` (GitLab), `fcli` (Figma)
- `SCRUTINY_GITHUB_REPO` overrides download repo (default `morphet81/scrutiny`)
- Binary fetch uses GitHub Release **latest** by default (cache keyed by `bin/.scrutiny-version`; refreshes when tag changes). Set `SCRUTINY_VERSION=0.1.5` only to pin. `SCRUTINY_USE_LOCAL=1` forces local `cargo` build.

## Build (developers)

```bash
cargo build --release
./target/release/scrutiny eval --help
./target/release/scrutiny forge-fetch --help
bash scripts/ensure-bin.sh
```

## Commands

### Review pipeline

```bash
./target/release/scrutiny eval
./target/release/scrutiny eval --base main --head abcdef0 --client claude
./target/release/scrutiny map --eval /tmp/scrutiny-…-eval.json
./target/release/scrutiny pack --map /tmp/scrutiny-…-map.json
./target/release/scrutiny scan --map … --pack … --eval …
# interactive: all six knobs in one session (or --from-json for CI)
./target/release/scrutiny plan-confirm --eval …
./target/release/scrutiny plan-write --eval … --map … --pack … --scan … \
  --answers /tmp/…-plan-answers.json
# after spawning reviewers/evangelists:
./target/release/scrutiny pack-partition --pack … --reviewers 2
./target/release/scrutiny review-session-write --plan … --pack … \
  --from-json '[{"role":"reviewer","index":1,"paths":["a.rs"],"findings_count":2}]'
./target/release/scrutiny findings-init --scan … --eval … --pack … --plan … [--pr 42]
# agent edits findings JSON during triage, then:
./target/release/scrutiny findings-resolve --findings …
./target/release/scrutiny findings-validate --findings …
./target/release/scrutiny post-comments --findings …
```

### plan-confirm / plan-write

`plan-confirm` always asks (stdin): model, security, performance, error-handling, reviewers, evangelists — defaults from eval `suggested_plan`. Prints answers JSON path. `plan-write --answers` (or `--from-json` of that shape) applies caps: `max_reviewers` when pack is small (`pack_chars < 4000` → 1), evangelists only with architecture risk / tier L+, `skip_ai` when XS+docs or zero agents. Plan stores both requested and effective counts.

### Review session

`pack-partition` splits pack slice paths across N reviewers (round-robin). `review-session-write` records spawned agents and **fails** if reviewer/evangelist counts do not match the plan — skill must re-spawn before triage.

### Findings / post-comments

After triage, findings live in a structured JSON file (`include`, `chosen_option`, `comment_body`, `anchor`, `review.event`). Severities: `critical` | `warning` | `info`.

`post-comments` requires a GitHub PR (`--pr` on init or `gh pr view` for the current branch). It **prompts** for `COMMENT` / `REQUEST_CHANGES` / `APPROVE` (or pass `--event`), then creates one PR review with line comments; bodies end with `[AI Agent]`.

If the authenticated user already has a **PENDING** review on that PR, the script asks:

1. Add these comments to the pending review, then submit it  
2. Close the pending review (choose event), then create a **new** review with the findings  

Agents must not call `gh` review create/dismiss/delete — only `post-comments`.

Line anchors are verified with `git show <head_oid>:<path>` — never invent line numbers.

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
