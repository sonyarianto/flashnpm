# flashnpm — Flash npm in idiomatic Rust

A fast, tiny npm-registry package manager in pure idiomatic Rust,
ported from the sibling [`upm`](https://github.com/unjs/upm) TypeScript project
(`../upm`, ~14k LOC src, zero-dep Node).

`upm` proves Node builtins are enough; `flashnpm` proves Rust's crates are better:
tokio concurrency, `reqwest` TLS, content-addressed store with hardlinks,
and a `node_modules/.flashnpm` layout with no hoisting — same semantics, native speed.

## Status: 0.3 (parity pass + concurrent resolve)

Verified live against `registry.npmjs.org`:

- `flashnpm install` / `i` / `ci` (`--frozen-lockfile`, `--verify`), `--production`, `--offline`, `--prefer-offline`
- `flashnpm add <spec>...` (`--dev/-D`, `--optional/-O`, `--exact/-E`, `-w`), `flashnpm remove` (`-w`)
- `flashnpm resolve`, `flashnpm fetch [--lock] [--production]`, `flashnpm lock`, `flashnpm dedupe`, `flashnpm prune`
- `flashnpm run [script] [--if-present] [-w/--workspaces/--include-workspace-root]`
  (bare `flashnpm <script>` implies `run`; `t`/`tst` are `test`),
  `flashnpm exec [-p ...] [-c ...]` and the `flashnpx` companion binary
- Tarball deps: `flashnpm add ./vendor/lib.tgz`, `foo@file:…`, `foo@https://…tgz`
  — keyed `name@<source>`, URLs pinned, local files re-locked on replace,
  frozen fails stale. Installs from npmjs, `file:` and URL entries link alike
- Workspaces: `workspaces: [...]` discovery (npm order, `!` negations),
  `workspace:` ranges (stay typed), bare names saving `^local`, `-w` targeting,
  whole-tree install from the root, `run --workspaces` in dependency order
  (failures don't stop others, first code wins)
- `flashnpm dedupe`: re-resolves preferring locked versions (lowest locked wins ties)
- `flashnpm.lock` v1 (own file; reads/writes like `upm.lock` but separate),
  lock stability (unrelated pins kept across re-resolves)
- 1-day `min-release-age` default, `--before`, `--min-release-age-exclude`,
  `.npmrc` hierarchy + `npm_config_*`
- Store at `~/.flashnpm/store` (`FLASHNPM_STORE` overrides; `--store` wins): `files/`
  blobs, `index/` per-package file lists (tarballs under `index/__tarball__/`),
  `metadata/` packuments with 5-min freshness + ETag revalidation
- Linker: `node_modules/.flashnpm/<entry>` with per-entry `node_modules` isolation
  (a package sees its own declared deps even under version conflicts), root
  links only its direct deps, per-workspace `node_modules` + `.bin`, entry
  sweep on remove/replace, install-state fast path (`already up to date`)
- `FLASHNPM_PROFILE=1`: `PHASE <label> <ms>` stderr marks for profiling installs
- `bench/bench.sh`: cold/warm/repeat harness, flashnpm vs upm with private stores

Performance (profiled cold `express@^4`, 71 packages): resolve dominated at
~4.5s of 5.6s (sequential packument RTTs), so each frontier's packuments now
fetch concurrently (16-wide) and edges process in pop order — resolve 4.6s →
1.7s with byte-identical lockfiles. Warm/repeat untouched (state fast path).
Lock output is deterministic under HashMap iteration order (root edges resolve
per declared range; 15/15 repeat installs byte-stable) — verified after catching
one nondeterministic root-edge bug with repeated runs.

Known deviations from upm (documented, not accidental):

- Registry lock edges keep their declared ranges (upm rewrites them to exact
  versions); pins still hold via the `keep` map, and frozen installs verify.
- No npm passthrough commands, foreign lockfiles, git/dir deps, lifecycle
  scripts, `storeBackend`, or `--verify` byte-hash audit (sizes/links/bins only).
- Failed optional branches fail the resolve (upm drops them) — recorded future work.

## Layout (mirrors `upm/src/*.ts`)

- `spec`, `semver` — spec parsing + npm range matching (`spec.ts`/`semver.ts`)
- `integrity` — SSRI subset (`integrity.ts`)
- `config` — `.npmrc` layers (`config.ts`)
- `registry` — packument client + metadata cache (`registry.ts`)
- `pick` — version selection + age gate (`pick.ts`)
- `tarball` — `file:`/`https:` deps (`tarball-deps.ts` + `Tarball` store half)
- `resolve` — flat walk, concurrent packument fetch, no hoisting (`resolve.ts`)
- `lock` — `flashnpm.lock` round-trip (`lock.ts`)
- `store` — fetch + unpack + index (`store.ts`, `unpack.ts`)
- `link` — `.flashnpm` linker + isolation + sweep + `--verify` (`link.ts`)
- `workspaces` — discovery + selection (`workspaces.ts`)
- `run`, `exec` — script running + bin lookup (`run.ts`, `exec.ts`)
- `state` — install-state fast path (`state.ts`)
- `api` — commands as functions, no printing (`api.ts`)
- `cli` — clap argv + output (`cli.ts`)

## Use

```sh
cargo build
./target/debug/flashnpm install --dir ./my-project
./target/debug/flashnpm add 'vue@^3' --dir ./my-project
./target/debug/flashnpm resolve 'nanoid@^5' --min-release-age 0
./target/debug/flashnpm install --frozen-lockfile
./target/debug/flashnpm run --workspaces build
flashnpx -p cowsay@1.6.0 cowsay hi
cargo test
./bench/bench.sh
```

Design constraints inherited from `upm/.agents/design.md`: lockfiles travel
(all platforms kept until install), workspace is a leaf (never a store entry),
tarball identity is its source, stability beats freshness (unrelated pins kept),
cached state is evidence not authority, threads never change the answer.
