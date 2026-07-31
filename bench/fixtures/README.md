# Bench fixtures

Smoke knobs for `scrutiny bench`.

```bash
scrutiny bench --workload both --fixtures true \
  --from-json "$(cat bench/fixtures/probe-knobs.json)" \
  --forge-from-json "$(cat bench/fixtures/forge-knobs.json)" \
  --model sonnet
```

`--fixtures true` (default) builds a disposable mini Cargo repo per arm under `--out`.
