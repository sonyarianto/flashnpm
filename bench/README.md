# bench

`bench.sh` times cold / warm / repeat installs per fixture.

```sh
./bench.sh
# FIXTURES="tiny small" REPEAT=3 ./bench.sh
# FLASHNPM_BIN=../target/release/flashnpm ./bench.sh
```

Rules:

- Separate private stores (`/tmp/bench-flashnpm-store`); cold drops lock, store
  and tree, warm drops only the tree, repeat keeps everything.
- `--min-release-age 0` so the calendar can't move the picked versions.
- Each run installs with its own lockfile and removes it afterwards, so
  runs don't share version choices.

Check the tree, not just the clock: after a run, `fixtures/<name>/flashnpm.lock`
(kept only on failure — the script cleans up) and `node_modules` must hold
the expected versions.
