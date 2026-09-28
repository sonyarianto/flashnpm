#!/usr/bin/env bash
# bench.sh — cold / warm / repeat installs: flashnpm vs upm (when present).
#
# Fairness (same rules as upm's bench/README): private stores per manager,
# same fixtures, lifecycle scripts already a non-feature in both.
# Uses --min-release-age 0 so results don't drift with the calendar.
set -u
cd "$(dirname "$0")"

FLASHNPM_BIN="${FLASHNPM_BIN:-../target/debug/flashnpm}"
UPM_BIN="${UPM_BIN:-../../upm/upm}"
FIXTURES="${FIXTURES:-tiny small}"
REPEAT="${REPEAT:-3}"

have_upm=0
if [ -x "$UPM_BIN" ]; then have_upm=1; fi
if [ ! -x "$FLASHNPM_BIN" ]; then
  echo "building flashnpm (debug)…"
  (cd .. && cargo build --quiet) || exit 1
fi

# time one install; prints seconds
time_install() {
  local bin="$1" dir="$2" store="$3"
  local start end
  start=$(date +%s%N)
  if [ "$bin" = "flashnpm" ]; then
    FLASHNPM_STORE="$store" "$FLASHNPM_BIN" install --dir "$dir" --store "$store" --min-release-age 0 --silent >/dev/null 2>&1
  else
    UPM_STORE="$store" node "$UPM_BIN" install --dir "$dir" --store "$store" --min-release-age 0 --silent >/dev/null 2>&1
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
  esac
}

printf '%-8s %-8s %10s %10s\n' fixture manager cold warm
for fx in $FIXTURES; do
  setup_fixture "$fx"
  for mgr in flashnpm $([ $have_upm = 1 ] && echo upm); do
    dir="fixtures/$fx"
    store="/tmp/bench-$mgr-store"
    # cold: no lock, no store, no tree
    rm -f "$dir/flashnpm.lock" "$dir/upm.lock"
    rm -rf "$store" "$dir/node_modules"
    cold=$(time_install "$mgr" "$dir" "$store")
    # keep the manager's own lock for warm, drop the tree only
    rm -rf "$dir/node_modules"
    warm=$(time_install "$mgr" "$dir" "$store")
    # repeat: everything kept
    rep=$(time_install "$mgr" "$dir" "$store")
    printf '%-8s %-8s %10ss %10ss  (repeat %ss)\n' "$fx" "$mgr" "$cold" "$warm" "$rep"
    rm -f "$dir/flashnpm.lock" "$dir/upm.lock"
    rm -rf "$dir/node_modules"
  done
done
echo
echo "stores: flashnpm=$(du -sh /tmp/bench-flashnpm-store 2>/dev/null | cut -f1)"
[ $have_upm = 1 ] && echo "stores: upm=$(du -sh /tmp/bench-upm-store 2>/dev/null | cut -f1)"
