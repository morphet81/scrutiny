# Scrutiny

AI code-review + ticket-implement skills with **Rust** helpers.

## Skills

| Skill | Command | Role |
|-------|---------|------|
| **scrutiny** | `/scrutiny` | Review local branch or PR (eval → map → pack → scan → AI) |
| **forge** | `/forge` | Implement Jira / GitHub / GitLab / inline ticket with multi-agent team |

## Install with `npx skills`

[`npx skills`](https://github.com/vercel-labs/skills) copies skill folders. It does **not** compile Rust; each skill’s `scripts/ensure-bin.sh` downloads a GitHub Release binary or builds from source on first use.

```bash
# both skills
npx skills add alexanderobellianne/scrutiny -g -y --skill '*'

# one skill
npx skills add alexanderobellianne/scrutiny@scrutiny -g -y
npx skills add alexanderobellianne/scrutiny@forge -g -y

# local checkout
npx skills add /path/to/scrutiny -g -y --skill '*' --agent cursor
```

Then `/scrutiny`, `/scrutiny <PR-URL>`, `/forge <ticket-URL>`, `/forge --inline <desc>`.

### Prerequisites

- `git`
- Network for release binary **or** Rust toolchain for `cargo build --release`
- Optional: `gh` (PR review + GitHub issues), `acli` (Jira), `glab` (GitLab), `fcli` (Figma)
- `SCRUTINY_GITHUB_REPO` / `SCRUTINY_VERSION` override binary fetch

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
./target/release/scrutiny plan-write --eval … --map … --pack … --scan … \
  --client claude --model sonnet \
  --security true --performance false --error-handling true \
  --reviewers 1 --evangelists 0
```

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

## Token-saving (forge)

- Artifact-first: agents get ticket/session/brief paths — no re-fetch CLIs
- `forge-brief` ~1–2KB instead of full ticket dumps
- `forge-context` keyword paths + test harness sniff
- Post-impl review reuses scrutiny pack (pack-only agents)
- Config force knobs skip prompt turns; disable Figma/lore when unused
- `reviewers = evangelists = 0` skips post-impl AI review

## Releases

**Not on crates.io** — both crates set `publish = false`; `release.toml` keeps `cargo release` from publishing.

Bump + tag (no registry):

```bash
cargo release patch --execute   # or minor / major
```

Tag `v*` runs `.github/workflows/release.yml` and uploads platform binaries; `ensure-bin.sh` downloads the host asset when present.

## Layout

```
skills/scrutiny/SKILL.md   # /scrutiny
skills/forge/SKILL.md      # /forge
scripts/ensure-bin.sh      # shared (also copied under each skill)
config/default.toml
crates/scrutiny-cli/
crates/scrutiny-core/
```
