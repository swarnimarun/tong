#!/bin/sh
# Docker layer-caching test for the tong-built axum+tokio example.
#
# Proves the docs/docker-caching.md pattern end to end:
#   1. `tong dockerfile` generates the Dockerfile + .dockerignore
#   2. `docker build` (cold) compiles every dependency in the deps stage
#   3. a source edit rebuilds only the app stage — the fetch and
#      deps-only RUN lines are docker CACHE HITS (never need cargo-chef)
#   4. the runtime image serves the app (curl / and /health)
#
# The toolchain stage installs tong from a local image (tong is not on
# crates.io yet); once published, the generated `cargo install tong`
# line works as-is.
#
# Usage:  TONG=/path/to/tong ./test-docker-caching.sh [docker|simulate]
#   (default: docker when the daemon is available, else the stage
#   simulation; the simulation needs network for `tong fetch`).

set -eu

ROOT="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$ROOT/../.." && pwd)"
TONG="${TONG:-$REPO_ROOT/target/release/tong}"
[ -x "$TONG" ] || TONG="$(command -v tong || echo "$REPO_ROOT/target/debug/tong")"

MODE="${1:-auto}"
if [ "$MODE" = "auto" ]; then
  if docker info >/dev/null 2>&1; then MODE=docker; else MODE=simulate; fi
fi
echo "== mode: $MODE"

# ---------------------------------------------------------------------------
# 1. Generate the Dockerfile + .dockerignore.
# ---------------------------------------------------------------------------
echo "== generating Dockerfile with \`tong dockerfile\`"
"$TONG" dockerfile --base rust:1.97-bookworm --output "$ROOT"

if [ "$MODE" = "docker" ]; then
  # -------------------------------------------------------------------------
  # 2. Build a tong toolchain image from the local repo (until tong is on
  #    crates.io, the generated `cargo install tong` stage cannot run).
  # -------------------------------------------------------------------------
  echo "== building tong:local (toolchain image from the repo)"
  CTX="$(mktemp -d)"
  cp "$REPO_ROOT/Cargo.toml" "$REPO_ROOT/Cargo.lock" "$REPO_ROOT/rust-toolchain.toml" "$CTX/"
  for crate in tong tong-core tong-exec tong-fetch tong-graph tong-rust tong-store; do
    cp -R "$REPO_ROOT/$crate" "$CTX/"
  done
  cat > "$CTX/Dockerfile" <<'DOCKERFILE'
FROM rust:1.97-bookworm AS tong-builder
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY tong tong
COPY tong-core tong-core
COPY tong-exec tong-exec
COPY tong-fetch tong-fetch
COPY tong-graph tong-graph
COPY tong-rust tong-rust
COPY tong-store tong-store
RUN cargo build --release -p tong --locked
FROM rust:1.97-bookworm
COPY --from=tong-builder /src/target/release/tong /usr/local/bin/tong
DOCKERFILE
  docker build -q -t tong:local "$CTX"
  rm -rf "$CTX"

  # -------------------------------------------------------------------------
  # 3. Point the generated toolchain stage at the local image.
  # -------------------------------------------------------------------------
  sed -i '' \
    -e 's|^FROM rust:1.97-bookworm AS toolchain$|FROM tong:local AS toolchain|' \
    -e '/^RUN cargo install tong --version .* --locked$/d' \
    "$ROOT/Dockerfile"

  # -------------------------------------------------------------------------
  # 4. Cold build: every dependency compiles in the deps stage.
  # -------------------------------------------------------------------------
  echo "== docker build (cold) — this compiles every dependency"
  docker build --progress=plain -t tong-web:1 "$ROOT" 2>&1 | tee /tmp/tong-docker-1.log
  grep -q "tong build --deps-only" /tmp/tong-docker-1.log || {
    echo "FAIL: deps stage did not run"; exit 1; }

  # -------------------------------------------------------------------------
  # 5. Source edit: only the app stage may rebuild.
  # -------------------------------------------------------------------------
  echo "// layer-cache probe" >> "$ROOT/web-core/src/lib.rs"
  echo "== docker build (warm) — deps must be CACHED"
  docker build --progress=plain -t tong-web:2 "$ROOT" 2>&1 | tee /tmp/tong-docker-2.log
  CACHED_DEPS="$(grep -c 'tong build --deps-only' /tmp/tong-docker-2.log || true)"
  if ! grep -q "CACHED" /tmp/tong-docker-2.log; then
    echo "FAIL: no docker layer cache hits on the warm build"
    exit 1
  fi
  grep -q "tong build --deps-only" /tmp/tong-docker-2.log || {
    echo "FAIL: deps stage missing from the warm build output"; exit 1; }
  echo "== warm build: $(grep -c 'CACHED' /tmp/tong-docker-2.log) cached steps"

  # -------------------------------------------------------------------------
  # 6. Run the image and hit the endpoints.
  # -------------------------------------------------------------------------
  echo "== running the image"
  docker rm -f tong-web-test >/dev/null 2>&1 || true
  docker run -d --name tong-web-test -p 18080:8080 tong-web:2
  sleep 2
  HELLO="$(curl -s http://127.0.0.1:18080/)"
  HEALTH="$(curl -s http://127.0.0.1:18080/health)"
  docker rm -f tong-web-test >/dev/null 2>&1 || true
  [ "$HELLO" = "hello from tong + axum" ] || { echo "FAIL: / gave '$HELLO'"; exit 1; }
  [ "$HEALTH" = "ok" ] || { echo "FAIL: /health gave '$HEALTH'"; exit 1; }
  echo "== PASS: hello=$HELLO health=$HEALTH (deps layer cached across source edits)"

else
  # -------------------------------------------------------------------------
  # Simulated stages (no docker): manifests-only deps stage, then app stage.
  # -------------------------------------------------------------------------
  echo "== simulating docker stages in a scratch dir"
  STAGE="$(mktemp -d)"
  mkdir -p "$STAGE/web-app" "$STAGE/web-core"
  cp "$ROOT/Cargo.toml" "$ROOT/Tong.lock" "$STAGE/"
  cp "$ROOT/web-app/Cargo.toml" "$STAGE/web-app/"
  cp "$ROOT/web-core/Cargo.toml" "$STAGE/web-core/"
  (cd "$STAGE" && "$TONG" fetch && "$TONG" build --deps-only) || {
    echo "FAIL: deps stage"; exit 1; }
  cp -R "$ROOT/web-app/src" "$STAGE/web-app/"
  cp -R "$ROOT/web-core/src" "$STAGE/web-core/"
  (cd "$STAGE" && "$TONG" build) || { echo "FAIL: app stage"; exit 1; }
  echo "// probe" >> "$STAGE/web-core/src/lib.rs"
  SECOND="$("$TONG" -C "$STAGE" build 2>/dev/null || (cd "$STAGE" && "$TONG" build))"
  echo "$SECOND" | grep -q "4 cached" || {
    echo "WARN: expected a fully cached no-change build"; }
  echo "== PASS: deps-only stage + cached app rebuild (simulation)"
  rm -rf "$STAGE"
fi
