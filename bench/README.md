# bench

`bench.sh` times cold / warm / repeat installs per fixture, flashnpm vs npm
vs upm vs snpm vs yarn vs pnpm (upm runs when `../../upm/upm` is executable,
snpm/yarn/pnpm when on `PATH`).

```sh
./bench.sh
# FIXTURES="tiny small" REPEAT=3 ./bench.sh
# FLASHNPM_BIN=../target/release/flashnpm UPM_BIN=../../upm/upm SNPM_BIN=snpm YARN_BIN=yarn PNPM_BIN=pnpm ./bench.sh
# MANAGERS="flashnpm npm upm snpm yarn pnpm" ./bench.sh
```

Rules:

- Separate private cache/store (`/tmp/bench-<mgr>-store`); cold drops lock,
  cache/store and tree, warm drops only the tree, repeat keeps everything.
  snpm's store is isolated via `SNPM_HOME`, yarn's via `--cache-folder`,
  pnpm's via `--store-dir`.
- flashnpm/upm run with `--min-release-age 0` so the calendar can't move the
  picked versions (npm/snpm/yarn/pnpm have no age gate by default, so
  versions may differ).
- Each manager installs with its own lockfile (`flashnpm.lock`,
  `package-lock.json`, `upm.lock`, `snpm-lock.yaml`, `yarn.lock`,
  `pnpm-lock.yaml`) and removes it afterwards, so runs don't share version
  choices.

Check the tree, not just the clock: after a run, `fixtures/<name>/*.lock*`
(kept only on failure — the script cleans up) and `node_modules` must hold
the expected versions.
