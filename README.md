# Scrutiny

CLI + agent skills that review PRs, implement tickets, and clear review comments — with as much work as possible in deterministic Rust, not in the model.

| Command | Does |
|---------|------|
| **`scrutiny probe`** | Review a local branch or GitHub PR |
| **`scrutiny forge`** | Implement a ticket (Jira / GitHub / GitLab / inline) |
| **`scrutiny parley`** | Fix unresolved PR review threads |
| **`scrutiny bench`** | Token-usage compare: cli vs skill vs skill+caveman |

Skills `/scrutiny`, `/forge`, `/parley` wrap the same flows for IDE agents.

Scripts write small JSON under `.scrutiny/`. Agents decide and edit; they do not re-explore the repo for plumbing.

---

## Install

```bash
brew tap morphet81/homebrew-tools
brew install scrutiny
# or: brew install morphet81/homebrew-tools/scrutiny

scrutiny skills-install -g -y --skill '*'
```

Upgrade: `brew update && brew upgrade scrutiny`.

**Need only skills** (binary already on PATH):

```bash
scrutiny skills-install -g -y --skill '*'
# or: npx skills add morphet81/scrutiny -g -y --skill '*'
```

**Prerequisites:** `git`. Optional: `gh` (PR + GitHub issues), `acli` (Jira), `glab` (GitLab), `fcli` (Figma). For probe/parley/forge agents: `claude`, `agent`/`cursor-agent`, and/or `codex` on PATH. `npx` for `skills-install`.

First run copies [`config/default.toml`](config/default.toml) → `~/.scrutiny/config.toml`. Add `.scrutiny/` to the repo `.gitignore` (CLI warns if missing).

---

## Quick start

### Probe — review

```bash
scrutiny probe
scrutiny probe --pr 42
scrutiny probe --client claude --spawn-mode isolated
```

Flow: detect agent CLI → eval / map / pack / scan → plan knobs → agents → triage → post comments to the PR.

Artifacts: `<repo>/.scrutiny/<pr>/` (or `.scrutiny/local/`). Config: `~/.scrutiny/config.toml`.

Resume triage/post from an existing AI report (skip analyze/agents):

```bash
scrutiny probe --from-report .scrutiny/42/report.json [--pr 42] [--scan .scrutiny/42/scan.json]
```

### Forge — implement

```bash
scrutiny forge PROJ-123
scrutiny forge "https://…/browse/PROJ-123"
scrutiny forge --inline --input "Add dark mode toggle"
```

Flow: fetch ticket → optional Figma → knobs (TDD, coverage, e2e, spawn) → optional TDD plan confirm → implement → verify gate (tests + pre-push checks) → commit → optional draft PR.

**Bulk** (many tickets, each on its own branch/worktree):

```bash
scrutiny forge bulk
scrutiny forge bulk --dry
scrutiny forge bulk --yes < tickets.txt
scrutiny forge bulk --concurrency 5
```

- `--yes` — stdin keys/URLs, no prompts, auto draft PRs  
- `--dry` — no agents / no real PRs; still creates worktrees; offers cleanup at end  
- `--concurrency N` — overrides `forge.bulk_concurrency`

### Parley — clear review comments

```bash
scrutiny parley
scrutiny parley --pr 42
```

Flow: fetch unresolved threads → fix agents → verifier → optional evangelist → pre-push gate → commit + push → reply under each thread.

Set `headless = false` to open each agent in a visible terminal (claude; tmux/zellij/macOS).

### Bench — token compare

Compare **cli** (`scrutiny probe` / `scrutiny forge`) vs **skill** (same agents + skill markdown preamble, no caveman) vs **skill-caveman** (preamble + caveman). Claude Code only — real `usage` from `--output-format json`.

```bash
scrutiny bench --workload both
scrutiny bench --workload probe --arms cli,skill-caveman --model sonnet
scrutiny bench --workload forge --fixtures true \
  --forge-from-json "$(cat bench/fixtures/forge-knobs.json)"
```

Default `--fixtures true` builds a disposable mini repo per arm under `.scrutiny/bench/<id>/`. Report: `bench-report.json` + table on stderr; path on stdout.

Env knobs used internally: `SCRUTINY_NO_CAVEMAN`, `SCRUTINY_BENCH_SKILL_PREAMBLE` (do not set by hand unless debugging).

---

## Config

Partial files are fine — every key has a default. Set only overrides.

| File | Role |
|------|------|
| `~/.scrutiny/config.toml` | Global (created from shipped defaults) |
| `<repo>/scrutiny.toml` | Per-project override (walks up from cwd) |

Merge is deep per key: local wins; tables merge key-by-key; scalars/arrays replace wholesale.

```toml
# scrutiny.toml — override only what this repo needs
default_client = "codex"

[models.claude]
m = "sonnet"

[git]
base_candidates = ["develop", "main"]
```

### Top-level

| Key | Default | Explanation |
|-----|---------|-------------|
| `default_client` | `"claude"` | AI client: `claude` \| `cursor` \| `codex` |
| `headless` | `true` | `true` = capture agent stdout. `false` = visible terminal window (claude; tmux/zellij/Terminal/iTerm2). Applies to probe, forge, parley |
| `force_client` | unset | Skip client prompt for `scrutiny probe` |
| `force_spawn_mode` | unset | Skip spawn prompt: `isolated` \| `team`. Unset → prompt (default **isolated**) |
| `editor` | unset | PR description editor; else `$VISUAL` → `$EDITOR` → `vi`. May include args (`"code --wait"`) |

### `[models.<client>]`

Model id per complexity tier for `claude` / `cursor` / `codex`.

| Key | Default (claude) | Default (cursor) | Default (codex) |
|-----|------------------|------------------|-----------------|
| `xs` | `haiku` | `composer-2-fast` | `gpt-5.3-codex` |
| `s` | `haiku` | `composer-2-fast` | `gpt-5.3-codex` |
| `m` | `sonnet` | `claude-4.6-sonnet-medium-thinking` | `gpt-5.5-medium` |
| `l` | `opus` | `claude-sonnet-5-thinking-high` | `gpt-5.6-sol-medium` |
| `xl` | `opus` | `claude-opus-4-8-thinking-high` | `gpt-5.6-terra-medium` |

Claude: Anthropic aliases or pinned ids — not Cursor slugs.

### `[review]`

Per-tier specialist toggles (`XS`…`XL` bools).

| Key | Default | Explanation |
|-----|---------|-------------|
| `security_by_tier` | XS/S off; M/L/XL on | Spawn security specialist when true for the eval tier |
| `performance_by_tier` | XS/S/M off; L/XL on | Spawn performance specialist |
| `error_handling_by_tier` | XS off; S–XL on | Spawn error-handling specialist |

### `[review.signals]`

Content-signal gating (path globs + diff regexes). Full lists live in [`config/default.toml`](config/default.toml).

| Key | Default | Explanation |
|-----|---------|-------------|
| `ignore_content_signals` | `false` | If `true`, tier toggles alone decide specialists (ignore path/diff hits) |
| `security_path_globs` | auth/api/secrets… | Paths that activate security review |
| `security_diff_patterns` | fetch/JWT/eval… | Diff regexes that activate security review |
| `performance_path_globs` | hooks/domain/stores… | Paths that activate performance review |
| `performance_diff_patterns` | useEffect/map/Mutex… | Diff regexes for performance |
| `performance_css_path_globs` | `**/*.{css,scss,sass,less}` | CSS paths for perf CSS checks |
| `performance_css_patterns` | nth-child/keyframes… | CSS diff regexes |
| `error_handling_diff_patterns` | try/catch/Result/unwrap… | Diff regexes for error-handling |

### `[agents]`

| Key | Default | Explanation |
|-----|---------|-------------|
| `max_agents_total` | `4` | Hard cap on concurrent review agents |
| `max_reviewers` | `2` | Cap on reviewers (also tightened when pack is small) |
| `max_evangelists` | `1` | Cap on evangelists |
| `reviewers_by_tier` | XS=0 S/M=1 L/XL=2 | Suggested reviewer count per tier |
| `evangelists_by_tier` | XS–M=0 L/XL=1 | Suggested evangelist count per tier |

### `[git]`

| Key | Default | Explanation |
|-----|---------|-------------|
| `base_candidates` | `["main","master","develop"]` | Branch names tried when resolving the review/forge base |
| `exclude_globs` | lockfiles, `node_modules`, `dist`, snaps… | Paths excluded from eval/pack |
| `artifact_globs` | `coverage/*`, playwright reports… | Never staged by forge/parley commits — cleaned from the tree instead |

### `[pack]`

Review-pack size and cross-file budgets.

| Key | Default | Explanation |
|-----|---------|-------------|
| `max_chars` | `48000` | Total pack character budget |
| `doc_digest_lines` | `40` | Max lines kept from doc files |
| `symbol_context_lines` | `3` | Context lines around changed symbols |
| `min_file_chars` | `1200` | Per-file floor before extra symbol bodies |
| `source_weight` | `4` | Budget weight for source files |
| `test_weight` | `2` | Budget weight for tests |
| `doc_weight` | `1` | Budget weight for docs |
| `enable_xref` | `true` | Resolve cross-file referenced signatures |
| `xref_max_symbols` | `40` | Max symbols resolved via xref |
| `xref_max_files_scanned` | `300` | Max files scanned for xref |
| `xref_char_budget` | `6000` | Char budget for xref snippets |
| `xref_body_lines` | `40` | Max body lines per xref hit |
| `annex_char_budget` | `12000` | Extra annex budget |

### `[pack.explore]`

Bounded agent exploration beyond the pack.

| Key | Default | Explanation |
|-----|---------|-------------|
| `enable` | `true` | Allow extra reads outside the pack |
| `max_extra_reads` | `6` | Cap on extra file reads |
| `max_extra_chars` | `24000` | Cap on chars from extra reads |
| `prefer_read_over_bash` | `true` | Prefer Read tool over shell |
| `allow_repo_grep` | `false` | Allow whole-repo grep |
| `require_pack_path_hint` | `true` | Extra reads must relate to a pack path |

### `[scan]` / `[scan.i18n]`

| Key | Default | Explanation |
|-----|---------|-------------|
| `scan.enable` | `true` | Run deterministic scan before AI review |
| `scan.commands` | `[]` | Extra lint commands (repo cwd) |
| `scan.i18n.enable` | `true` | Check locale JSON catalogs |
| `scan.i18n.reference_locale` | `"en"` | Locale other files must cover |
| `scan.i18n.path_globs` | `**/locales/*.json` etc. | Where locale files live |
| `scan.i18n.check_placeholders` | `true` | Flag mismatched `{placeholders}` |
| `scan.i18n.check_empty_values` | `true` | Flag empty translation values |
| `scan.i18n.full_catalog` | `false` | Compare full catalogs vs change-scoped |

### `[forge]`

| Key | Default | Explanation |
|-----|---------|-------------|
| `approach` | unset | Force `tdd` \| `heads_down` \| `plan`; omit → prompt |
| `e2e` | unset | Force e2e on/off; omit → prompt |
| `agents` / `testers` / `reviewers` / `evangelists` | unset | Force team counts; omit → prompt |
| `model` | unset | Pin model id; omit → complexity suggestion + prompt |
| `enable_figma` | `true` | Export Figma when ticket has links (`fcli`) |
| `enable_lore` | `true` | Include project lore in context |
| `enable_ticket_writeback` | `true` | Allow writing back to the ticket |
| `enable_po` | `true` | Enable PO/team planning roles |
| `enable_branch` | `true` | Interactive branch / worktree step |
| `default_approach` | `"tdd"` | Default when prompting |
| `default_agents` | `2` | Default developer agents |
| `default_testers` | `1` | Default testers |
| `default_reviewers` | `1` | Default reviewers |
| `default_evangelists` | `0` | Default evangelists |
| `verify_commands` | `[]` | Explicit verify-gate commands; empty → auto-detect harness |
| `verify_max_loops` | `2` (shipped) | Max fix loops before gate fails (`5` if key omitted from a minimal file) |
| `verify_coverage` | `true` | Gate on coverage % when measurable |
| `prepush_cmd` | unset | Override pre-push checks in the verify gate; empty → `git hook run pre-push` if a hook exists |
| `branch_headless` | `"auto"` | `"auto"` follow detection \| `"never"` stay on current branch |
| `bulk_concurrency` | `3` | Max concurrent `forge bulk` items (`--concurrency` overrides) |
| `pr_description_prompt` | unset | If set, dedicated agent writes PR body from this prompt + diff |

### `[forge.complexity]`

Ticket scoring → tier → `[models.<client>]`. Lists default in `config/default.toml`.

| Key | Default | Explanation |
|-----|---------|-------------|
| `story_point_fields` | `story_points`, Jira customfields… | Fields tried for story points |
| `breadth_keywords` | refactor, migrate… | +8 pts/hit (cap 2) |
| `integration_keywords` | api, webhook… | +6 pts/hit (cap 2) |
| `risk_keywords` | auth, payment… | +10 pts/hit (cap 2) |
| `trivial_keywords` | typo, minor… | −8 pts/hit (cap 2) |
| `bump_labels` | urgent, epic… | +6 pts (max 1) |
| `lower_labels` | trivial, small… | −6 pts (max 1) |
| `tier_thresholds` | `[18,35,55,95]` | Inclusive upper bounds for XS/S/M/L; above → XL |

Also scored (not configurable lists): AC count, description size, issue type, Figma URLs, comment volume.

### `[parley]`

| Key | Default | Explanation |
|-----|---------|-------------|
| `default_members` | `1` | Default fix agents (capped by comment count) |
| `default_verifiers` | `1` | Agents that check fixes actually address threads |
| `default_evangelists` | `1` | Optional architecture/quality pass (isolated) |
| `repair` | `true` | Re-implement stubs / verifier rejects before replies |
| `prepush_cmd` | unset | Override quiet pre-push gate command; else repo hook |
| `prepush_fix_max_loops` | `5` | Max check → fix-agent → re-check cycles in the gate |
| `push_fix_max_loops` | `2` | **Deprecated** — use `prepush_fix_max_loops` |
| `agent_wall_secs` | unset | **Deprecated** — use `[timeouts].parley_agent_wall_secs` |
| `prepush_fix_wall_secs` | unset | **Deprecated** — use `[timeouts].parley_prepush_fix_wall_secs` |

### `[timeouts]`

Seconds. `agent_wall_secs` is the base; unset stages derive from it (`0` = unset). A timeout kills the agent; stage reports `TIMEOUT`.

| Key | Default | Explanation |
|-----|---------|-------------|
| `agent_wall_secs` | `600` | Base for every stage below |
| `progress_secs` | `15` | “Still running” tick interval |
| `nonheadless_wall_secs` | base ×3 (`1800`) | Agents in a visible terminal |
| `probe_isolated_wall_secs` | base | Isolated probe agents |
| `probe_team_wall_secs` | base | Team-mode probe lead |
| `probe_consolidate_wall_secs` | base | Isolated consolidate pass |
| `probe_ask_wall_secs` | base | Triage “Ask a question…” |
| `forge_test_plan_wall_secs` | base | TDD test-plan agent |
| `forge_pr_description_wall_secs` | base | PR description agent |
| `forge_implement_wall_secs` | base ×2 (`1200`) | Implement agent |
| `forge_fix_wall_secs` | base ×2 (`1200`) | Verify-gate fix agent |
| `forge_bulk_item_wall_secs` | base ×8 (`4800`) | One bulk-forge item |
| `parley_agent_wall_secs` | base | Member / verifier / evangelist |
| `parley_prepush_fix_wall_secs` | base ×2 (`1200`) | Pre-push fix agent |

```toml
[timeouts]
forge_implement_wall_secs = 3600
```

### `[prompts]`

Inject text into spawned-agent prompts without rebuilding. Order: **`global` → `<role>` → scrutiny’s prompt**.

| Key | Default | Explanation |
|-----|---------|-------------|
| `global` | `""` | Prepended to every spawned agent |
| `agents.<role>` | unset | Prepended for one role only |

Role key = agent label with `-` → `_`. Unknown keys ignored. Team mode: only the lead’s prompt is injected — not sub-agents the lead spawns.

| Surface | Role keys |
|---------|-----------|
| probe | `reviewer`, `evangelist`, `security`, `performance`, `error_handling`, `lead` |
| parley | `parley_member`, `parley_lead`, `parley_verifier`, `parley_evangelist`, `parley_push_fix` |
| forge | `forge_test_plan`, `forge_test_plan_revise`, `forge_implement`, `forge_po_team`, `forge_verify_fix` |

```toml
[prompts]
global = "Cite file:line. Never edit generated code."

[prompts.agents]
reviewer = "Prioritise null-safety and async races."
forge_implement = "Match existing Result/anyhow style."
```

---

## Details

### Spawn modes (probe / parley)

| Mode | Behavior |
|------|----------|
| **isolated** (default) | Script runs reviewers/evangelists/specialists in parallel; script collates + dedupes. Lower token cost |
| **team** | One lead agent embeds role briefs, spawns members, returns one findings JSON. Higher token cost |

```bash
scrutiny agent-prompt --role reviewer --pack .scrutiny/42/pack.json
scrutiny agent-prompt --role lead --pack .scrutiny/42/pack.json --plan .scrutiny/42/plan.json
```

### Probe step pipeline

For IDE chaining / debugging (one-shot `probe` already runs this):

```bash
scrutiny eval [--base main --head <sha> --pr 42]
scrutiny map --eval .scrutiny/42/eval.json
scrutiny pack --map .scrutiny/42/map.json
scrutiny scan --map … --pack … --eval …
scrutiny plan-confirm --eval …
scrutiny plan-write --eval … --map … --pack … --scan … --answers …
scrutiny findings-init … && scrutiny findings-triage … && scrutiny post-comments …
```

`eval` scores XS…XL from diff size/scatter/risk. Docs are listed but not scored. Comment-only LOC is stripped.

`plan-confirm` asks model, security, performance, error-handling, reviewers, evangelists, spawn_mode. `plan-write` applies caps (small pack → ≤1 reviewer; evangelists only with architecture risk / tier L+; `skip_ai` for XS+docs).

### Findings & post-comments

Triage (TTY): ↑/↓ — Post / Ignore / Ask… (or fix option A/B…). Ask is its own menu item; Q&A stored on `ask_log`. Non-TTY: `P` / `I` / option letter / `ask <q>`.

Severities: `critical` \| `warning` \| `suggestion`. Bodies end with `[AI Agent]`.

| Placement | When |
|-----------|------|
| Line | Path + line on PR/pack diff |
| File | Path but line not commentable |
| Global | No path → `### Global notes` in review body |

Pending review on the PR → choose append-then-submit or close-and-recreate. Transient GitHub failures retry (4×, backoff). Resume failed posts with the same `post-comments` command — already-posted comments are skipped.

### Forge / parley discrete commands

```bash
scrutiny forge-fetch --input "…"
scrutiny forge-plan-write --ticket … --client … --model … --approach tdd …
scrutiny forge-context --ticket …
scrutiny forge-brief --ticket … --session … --context …

scrutiny parley-fetch …
scrutiny parley-plan-write …
scrutiny parley-reply …
```

Each prints one JSON path on stdout.

### Pre-push gate

Forge verify and parley both run the repo’s pre-push checks quietly (log file, not a pane flood). Override with `forge.prepush_cmd` / `parley.prepush_cmd`. No hook and no override → gate is a no-op green.

### Claude auth

Log in once (`claude` → `/login`). Probe does not pass `--bare` unless `ANTHROPIC_API_KEY` is set or `SCRUTINY_CLAUDE_BARE=1`. Force OAuth with a key present: `SCRUTINY_CLAUDE_NO_BARE=1`.

### Token-saving habits

- Prefer **isolated** spawn  
- Force knobs in config to skip prompts  
- Turn off Figma/lore when unused; set reviewers/evangelists to `0` for static-only  
- Tune `[review.signals]`, `[pack.explore]`, `[agents].max_agents_total`  
- Locale files are not AI-reviewed — `scan.i18n` covers them  

### Build (developers)

```bash
cargo build --release
./target/release/scrutiny probe --help
bash scripts/ensure-bin.sh
```

Binary fetch (non-brew): GitHub Release **latest** by default. Pin with `SCRUTINY_VERSION=…`. Force local build: `SCRUTINY_USE_LOCAL=1`. Override repo: `SCRUTINY_GITHUB_REPO` (default `morphet81/scrutiny`).

### Releases

Not on crates.io (`publish = false`). Tag with:

```bash
cargo release patch --execute
```

Tag `v*` builds platform binaries. Targets: `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`. Intel Mac builds via cargo in `ensure-bin`.

### Layout

```
skills/scrutiny/SKILL.md
skills/forge/SKILL.md
skills/parley/SKILL.md
config/default.toml
crates/scrutiny-cli/
crates/scrutiny-core/
scripts/ensure-bin.sh
```
