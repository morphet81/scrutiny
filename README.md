# Scrutiny

AI code-review skill with **Rust** helpers:

1. `scrutiny eval` — complexity vs auto-detected base → temp JSON  
2. `scrutiny map` — change map → temp JSON  
3. `scrutiny pack` — diff + symbol slices + doc digests (AI reads this, not full files)  
4. `scrutiny scan` — deterministic findings (zero-token)  
5. `scrutiny plan-write` — confirmed plan + `skip_ai` short-circuit  
6. Skill (`SKILL.md`) — confirm plan, optional AI on pack only, caveman findings, triage

## Install with `npx skills`

[`npx skills`](https://github.com/vercel-labs/skills) copies this repo (root `SKILL.md`) into your agent skills folder. It does **not** compile Rust; `scripts/ensure-bin.sh` downloads a GitHub Release binary or builds from source on first use.

```bash
npx skills add alexanderobellianne/scrutiny -g -y
npx skills add /path/to/scrutiny -g -y --agent cursor
npx skills add alexanderobellianne/scrutiny -g -y --agent claude-code
```

Then `/scrutiny` or `/scrutiny <PR-URL>`.

### Prerequisites

- `git`
- Network for release binary **or** Rust toolchain for `cargo build --release`
- Optional: `gh` for PR mode
- `SCRUTINY_GITHUB_REPO` / `SCRUTINY_VERSION` override binary fetch

## Build (developers)

```bash
cargo build --release
./target/release/scrutiny eval --help
bash scripts/ensure-bin.sh
```

## Commands

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

Each command prints **one** temp JSON path on stdout.

## Config

First run copies `config/default.toml` → `~/.scrutiny/config.toml`.

- `[models.claude]` — Claude Code aliases (`haiku`/`sonnet`/`opus`) or pinned ids (`claude-sonnet-4-6`). Not Cursor slugs.
- `[pack]` — `max_chars`, doc digest / symbol context
- `[scan]` — enable + optional lint `commands`

## Releases

**Not on crates.io** — both crates set `publish = false`; `release.toml` keeps `cargo release` from publishing.

Bump + tag (no registry):

```bash
cargo release patch --execute   # or minor / major
```

Tag `v*` runs `.github/workflows/release.yml` and uploads platform binaries; `ensure-bin.sh` downloads the host asset when present.
