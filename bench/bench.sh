#!/usr/bin/env bash
# bench.sh — cold / warm / repeat installs: flashnpm vs npm vs upm vs snpm
# vs yarn vs pnpm.
#
# Private cache/store per manager, same fixtures.
# flashnpm/upm use --min-release-age 0 so results don't drift with the calendar
# (npm/snpm/yarn/pnpm have no age gate by default, so picked versions may differ).
set -u
cd "$(dirname "$0")"

FLASHNPM_BIN="${FLASHNPM_BIN:-../target/debug/flashnpm}"
UPM_BIN="${UPM_BIN:-../../upm/upm}"
SNPM_BIN="${SNPM_BIN:-snpm}"
YARN_BIN="${YARN_BIN:-yarn}"
PNPM_BIN="${PNPM_BIN:-pnpm}"
FIXTURES="${FIXTURES:-tiny small}"
REPEAT="${REPEAT:-3}"
MANAGERS="${MANAGERS:-flashnpm npm upm snpm yarn pnpm}"

have_npm=0
if command -v npm >/dev/null 2>&1; then have_npm=1; fi
have_upm=0
if [ -x "$UPM_BIN" ]; then have_upm=1; fi
have_snpm=0
case "$SNPM_BIN" in
  */*) [ -x "$SNPM_BIN" ] && have_snpm=1 ;;
  *) command -v "$SNPM_BIN" >/dev/null 2>&1 && have_snpm=1 ;;
esac
have_yarn=0
case "$YARN_BIN" in
  */*) [ -x "$YARN_BIN" ] && have_yarn=1 ;;
  *) command -v "$YARN_BIN" >/dev/null 2>&1 && have_yarn=1 ;;
esac
have_pnpm=0
case "$PNPM_BIN" in
  */*) [ -x "$PNPM_BIN" ] && have_pnpm=1 ;;
  *) command -v "$PNPM_BIN" >/dev/null 2>&1 && have_pnpm=1 ;;
esac
if [ ! -x "$FLASHNPM_BIN" ]; then
  echo "building flashnpm (debug)…"
  (cd .. && cargo build --quiet) || exit 1
fi

# time one install; prints seconds
time_install() {
  local mgr="$1" dir="$2" store="$3"
  local start end
  start=$(date +%s%N)
  if [ "$mgr" = "flashnpm" ]; then
    FLASHNPM_STORE="$store" "$FLASHNPM_BIN" install --dir "$dir" --store "$store" --min-release-age 0 --silent >/dev/null 2>&1
  elif [ "$mgr" = "upm" ]; then
    UPM_STORE="$store" node "$UPM_BIN" install --dir "$dir" --store "$store" --min-release-age 0 --silent >/dev/null 2>&1
  elif [ "$mgr" = "snpm" ]; then
    # snpm is cwd-based (no --dir flag); isolate its store via SNPM_HOME
    (cd "$dir" && SNPM_HOME="$store" "$SNPM_BIN" install >/dev/null 2>&1)
  elif [ "$mgr" = "yarn" ]; then
    "$YARN_BIN" install --cwd "$dir" --cache-folder "$store" --silent --no-progress --non-interactive >/dev/null 2>&1
  elif [ "$mgr" = "pnpm" ]; then
    "$PNPM_BIN" install --dir "$dir" --store-dir "$store" --silent >/dev/null 2>&1
  else
    npm install --prefix "$dir" --cache "$store" --no-audit --no-fund --loglevel=error >/dev/null 2>&1
  fi
  end=$(date +%s%N)
  awk "BEGIN {printf \"%.2f\", ($end - $start) / 1e9}"
}

setup_fixture() {
  local name="$1"
  local dir="fixtures/$name"
  mkdir -p "$dir"
  case "$name" in
    tiny)  echo '{"name":"tiny","version":"1.0.0","dependencies":{"nanoid":"^5"}}' > "$dir/package.json" ;;
    small) echo '{"name":"small","version":"1.0.0","dependencies":{"nanoid":"^5","is-odd":"^3"}}' > "$dir/package.json" ;;
    medium) echo '{"name":"medium","version":"1.0.0","dependencies":{"express":"^4"}}' > "$dir/package.json" ;;
    big) echo '{"name":"big","version":"1.0.0","dependencies":{"next":"^16"}}' > "$dir/package.json" ;;
  esac
}

lockfile_for() {
  case "$1" in
    flashnpm) echo "flashnpm.lock" ;;
    npm) echo "package-lock.json" ;;
    upm) echo "upm.lock" ;;
    snpm) echo "snpm-lock.yaml" ;;
    yarn) echo "yarn.lock" ;;
    pnpm) echo "pnpm-lock.yaml" ;;
  esac
}

clean_snpm_state() {
  # extra snpm state beyond snpm-lock.yaml
  rm -f "$1/snpm-lock.bin"
  rm -rf "$1/.snpm" "$1/.snpm-install-state"
}

printf '%-8s %-8s %10s %10s\n' fixture manager cold warm
for fx in $FIXTURES; do
  setup_fixture "$fx"
  for mgr in $MANAGERS; do
    if [ "$mgr" = "npm" ] && [ "$have_npm" = 0 ]; then
      echo "skipping npm (not found)" >&2
      continue
    fi
    if [ "$mgr" = "upm" ] && [ "$have_upm" = 0 ]; then
      echo "skipping upm ($UPM_BIN not executable)" >&2
      continue
    fi
    if [ "$mgr" = "snpm" ] && [ "$have_snpm" = 0 ]; then
      echo "skipping snpm ($SNPM_BIN not found)" >&2
      continue
    fi
    if [ "$mgr" = "yarn" ] && [ "$have_yarn" = 0 ]; then
      echo "skipping yarn ($YARN_BIN not found)" >&2
      continue
    fi
    if [ "$mgr" = "pnpm" ] && [ "$have_pnpm" = 0 ]; then
      echo "skipping pnpm ($PNPM_BIN not found)" >&2
      continue
    fi
    dir="fixtures/$fx"
    store="/tmp/bench-$mgr-store"
    lock="$(lockfile_for "$mgr")"
    # cold: no lock, no cache/store, no tree
    rm -f "$dir/flashnpm.lock" "$dir/package-lock.json" "$dir/upm.lock" "$dir/snpm-lock.yaml" "$dir/yarn.lock" "$dir/pnpm-lock.yaml"
    clean_snpm_state "$dir"
    rm -rf "$store" "$dir/node_modules"
    cold=$(time_install "$mgr" "$dir" "$store")
    # keep the manager's own lock for warm, drop the tree only
    rm -rf "$dir/node_modules"
    warm=$(time_install "$mgr" "$dir" "$store")
    # repeat: everything kept
    rep=$(time_install "$mgr" "$dir" "$store")
    printf '%-8s %-8s %10ss %10ss  (repeat %ss)\n' "$fx" "$mgr" "$cold" "$warm" "$rep"
    rm -f "$dir/$lock"
    if [ "$mgr" = "snpm" ]; then clean_snpm_state "$dir"; fi
    rm -rf "$dir/node_modules"
  done
done
echo
echo "stores: flashnpm=$(du -sh /tmp/bench-flashnpm-store 2>/dev/null | cut -f1)"
[ "$have_npm" = 1 ] && echo "caches: npm=$(du -sh /tmp/bench-npm-store 2>/dev/null | cut -f1)"
[ "$have_upm" = 1 ] && echo "stores: upm=$(du -sh /tmp/bench-upm-store 2>/dev/null | cut -f1)"
[ "$have_snpm" = 1 ] && echo "stores: snpm=$(du -sh /tmp/bench-snpm-store 2>/dev/null | cut -f1)"
[ "$have_yarn" = 1 ] && echo "caches: yarn=$(du -sh /tmp/bench-yarn-store 2>/dev/null | cut -f1)"
[ "$have_pnpm" = 1 ] && echo "stores: pnpm=$(du -sh /tmp/bench-pnpm-store 2>/dev/null | cut -f1)"
