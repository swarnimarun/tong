#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tong=${TONG_BIN:-"$root/target/release/tong"}
fixture="$root/examples/09-cache-stability"
work=${TONG_STABILITY_DIR:-"$root/target/cache-stability"}

case "$tong" in
  */release/tong) ;;
  *) echo "cache-stability: TONG_BIN must be a release Tong binary, got $tong" >&2; exit 2 ;;
esac
test -x "$tong" || {
  echo "cache-stability: build first with: cargo build --release -p tong" >&2
  exit 2
}

rm -rf "$work"
mkdir -p "$work/workspace"
cp -R "$fixture/." "$work/workspace/"
store="$work/store"
log="$work/run.log"

run_build() {
  : >"$log"
  TONG_STORE_DIR="$store" "$tong" -v check --workspace --all-targets --offline \
    >"$work/stdout.log" 2>"$log"
}

decisions() {
  sed -n \
    -e 's/^  \[[0-9][0-9]*\/[0-9][0-9]*\] \([^ ]*\).* \[cached\]$/cached \1/p' \
    -e 's/^  Executed \[[0-9][0-9]*\/[0-9][0-9]*\] \([^ ]*\).*$/executed \1/p' \
    "$log" | LC_ALL=C sort
}

expect_all_cached() {
  scenario=$1
  if decisions | grep -q '^executed '; then
    echo "cache-stability: $scenario unexpectedly executed actions:" >&2
    decisions >&2
    exit 1
  fi
}

cd "$work/workspace"
run_build
run_build
run_build
expect_all_cached no-op

touch leaf-a/src/lib.rs
run_build
expect_all_cached mtime-only

printf 'changed but unused\n' >leaf-a/src/unused.rs
run_build
expect_all_cached unused-rust-file

printf 'changed but unrelated\n' >shared/unrelated.txt
run_build
expect_all_cached unrelated-build-script-file

cp leaf-b/src/lib.rs "$work/original.rs"
printf 'pub fn value() -> u32 { cache_shared::shared() + 3 }\n' >leaf-b/src/lib.rs
# Restore the original timestamp: ctime/file identity must still reveal the
# content edit to the snapshot index.
touch -r "$work/original.rs" leaf-b/src/lib.rs
run_build
decisions >"$work/leaf-edit.decisions"
sed -n 's/^executed /executed /p' "$work/leaf-edit.decisions" >"$work/leaf-edit.executed"
printf '%s\n' \
  'executed rust:bin:cache-app:cache-app' \
  'executed rust:lib:cache-leaf-b:rlib' \
  'executed rust:test-compile:cache-app:bench:smoke' \
  'executed rust:test-compile:cache-app:test:smoke' \
  'executed rust:test-compile:cache-leaf-b:lib:cache_leaf_b' \
  >"$work/leaf-edit.expected"
if ! cmp -s "$work/leaf-edit.expected" "$work/leaf-edit.executed"; then
  echo "cache-stability: preserved-mtime leaf edit had the wrong miss set" >&2
  echo "expected:" >&2
  sed 's/^/  /' "$work/leaf-edit.expected" >&2
  echo "actual:" >&2
  cat "$work/leaf-edit.decisions" >&2
  exit 1
fi

cp "$work/original.rs" leaf-b/src/lib.rs
run_build
run_build
expect_all_cached edit-revert

printf 'cache-stability: PASS\n'
printf 'decision report: %s\n' "$work/leaf-edit.decisions"
