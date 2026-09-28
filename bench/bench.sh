#!/usr/bin/env bash
# bench.sh — cold / warm / repeat installs for flashnpm.
#
# Private store per run, same fixtures.
# Uses --min-release-age 0 so results don't drift with the calendar.
set -u
cd "$(dirname "$0")"

FLASHNPM_BIN="${FLASHNPM_BIN:-../target/debug/flashnpm}"
FIXTURES="${FIXTURES:-tiny small}"
REPEAT="${REPEAT:-3}"

if [ ! -x "$FLASHNPM_BIN" ]; then
  echo "building flashnpm (debug)…"
  (cd .. && cargo build --quiet) || exit 1
fi

# time one install; prints seconds
time_install() {
  local dir="$1" store="$2"
  local start end
  start=$(date +%s%N)
  FLASHNPM_STORE="$store" "$FLASHNPM_BIN" install --dir "$dir" --store "$store" --min-release-age 0 --silent >/dev/null 2>&1
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
  dir="fixtures/$fx"
  store="/tmp/bench-flashnpm-store"
  # cold: no lock, no store, no tree
  rm -f "$dir/flashnpm.lock"
  rm -rf "$store" "$dir/node_modules"
  cold=$(time_install "$dir" "$store")
  # keep the lock for warm, drop the tree only
  rm -rf "$dir/node_modules"
  warm=$(time_install "$dir" "$store")
  # repeat: everything kept
  rep=$(time_install "$dir" "$store")
  printf '%-8s %-8s %10ss %10ss  (repeat %ss)\n' "$fx" "flashnpm" "$cold" "$warm" "$rep"
  rm -f "$dir/flashnpm.lock"
  rm -rf "$dir/node_modules"
done
echo
echo "stores: flashnpm=$(du -sh /tmp/bench-flashnpm-store 2>/dev/null | cut -f1)"
