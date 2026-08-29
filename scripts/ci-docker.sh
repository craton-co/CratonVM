#!/usr/bin/env bash
# Run an approximation of .github/workflows/ci.yml locally, in Docker, split
# into 4 shards: 3 covering `cargo test --workspace` (the actual test suite,
# ~19,400 #[test] functions across 22 crates) and a 4th covering the other
# CI checks that are cheap enough to run without external secrets, JDK-image
# matrices, or a nightly toolchain.
#
# Usage:
#   scripts/ci-docker.sh                 # build the image, run all 4 shards in parallel
#   scripts/ci-docker.sh --shard 2       # run just one shard (1-4)
#   scripts/ci-docker.sh --sequential    # run shards one at a time instead of in parallel
#   scripts/ci-docker.sh --no-build      # skip the `docker build` step (image already built)
#
# Requires Docker. Each shard runs in its own container against a shared
# cargo-registry cache volume and a per-shard target/ volume (kept separate
# so 4 concurrent `cargo` processes never lock-contend on the same
# incremental-build directory) — neither volume touches the host's own
# target/, so this never collides with a native Windows/macOS build.
#
# WHAT THE 3 TEST SHARDS ARE BALANCED ON. There is no measured per-test
# timing data for this workspace, so "run in +- the same time" is
# approximated by splitting workspace crates into 3 groups with roughly
# equal #[test] COUNTS (computed once via `grep -rc '#\[test\]'`), not
# measured wall-clock time. Count and time correlate but are not the same
# thing — `vm` and `native-builtins` carry the two largest, slowest
# integration-test binaries in the workspace, so shards 1-2 (which each
# anchor on one of those two crates) are the ones most likely to run long
# regardless of count. If a real skew shows up across actual runs, rebalance
# the SHARD*_CRATES arrays below rather than trusting the count split
# blindly.
set -u
set -o pipefail

# Git Bash on Windows rewrites any argument that looks like an absolute
# POSIX path (`/workspace`, `/usr/local/cargo/registry`, ...) into a Windows
# path before handing it to docker.exe, which is not an MSYS program and
# does not want that rewrite -- `-w /workspace` becomes `-w 'C:/Program
# Files/Git/workspace'` and the run fails with "the working directory ... is
# invalid". This opts every path argument in this script out of that
# rewrite; it is a no-op on Linux/macOS, where MSYS_NO_PATHCONV is unread.
export MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/.." && pwd)"
IMAGE="cratonvm-ci-docker"
OUT_DIR="$REPO_ROOT/target/ci-docker"
CARGO_CACHE_VOLUME="cratonvm-ci-cargo-registry"

# --- test-suite shards, by workspace package name -------------------------
# ~6643 #[test] functions
SHARD1_CRATES=(cratonvm-vm cratonvm-jit-cuda cratonvm-cli cratonvm-difftest
  cratonvm-native-collections cratonvm-jit-api libcratonvm
  cratonvm-native-builtins-crypto cratonvm-cuda-bridge
  cratonvm-native-builtins-security cratonvm-embed cratonvm-gpu)
# ~6585 #[test] functions
SHARD2_CRATES=(cratonvm-native-builtins cratonvm-classloading cratonvm-native-awt cratonvm-jfr)
# ~6172 #[test] functions
SHARD3_CRATES=(cratonvm-jit cratonvm-gc cratonvm-types cratonvm-native-io cratonvm-native-api cratonvm-reader)

usage() {
  cat >&2 <<'EOF'
usage: scripts/ci-docker.sh [--shard N] [--sequential] [--no-build] [-h|--help]

  --shard N       Run only shard N (1, 2, 3, or 4). Default: run all 4.
  --sequential    Run shards one after another instead of in parallel.
  --no-build      Skip `docker build` (assumes the image already exists).
  -h, --help      Show this help.

Shard 1-3: `cargo test` over a fixed subset of workspace crates (see the
           SHARD*_CRATES arrays at the top of this script).
Shard 4:   fmt, clippy, build, doc, and the fast standalone CI gates that
           need no JDK-image matrix, external secret, or nightly toolchain.
           Does NOT attempt to reproduce jdk-only, fuzz-smoke, miri, or
           publish-libcratonvm — those need a real JDK image matrix / a
           nightly + Miri toolchain / release packaging respectively, which
           this local runner does not set up. See ci.yml directly for those.

Logs land in target/ci-docker/shard-N.log; a summary prints at the end.
EOF
}

SHARD_FILTER=""
PARALLEL=1
DO_BUILD=1
while [ $# -gt 0 ]; do
  case "$1" in
    --shard) SHARD_FILTER="$2"; shift 2 ;;
    --sequential) PARALLEL=0; shift ;;
    --no-build) DO_BUILD=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 1 ;;
  esac
done

mkdir -p "$OUT_DIR"

if [ "$DO_BUILD" -eq 1 ]; then
  echo "[ci-docker] building image ($IMAGE) ..." >&2
  docker build -t "$IMAGE" -f "$HERE/docker/Dockerfile" "$HERE/docker" || exit 1
fi

docker volume create "$CARGO_CACHE_VOLUME" >/dev/null

run_in_container() {
  local shard_name="$1" target_volume="$2"
  shift 2
  docker volume create "$target_volume" >/dev/null
  docker run --rm \
    -v "$REPO_ROOT":/workspace \
    -v "$CARGO_CACHE_VOLUME":/usr/local/cargo/registry \
    -v "$target_volume":/workspace/target \
    -w /workspace \
    -e CARGO_TERM_COLOR=always \
    "$IMAGE" bash -lc "$*"
}

shard1() {
  run_in_container shard1 cratonvm-ci-target-1 \
    "cargo test $(printf -- '-p %s ' "${SHARD1_CRATES[@]}")"
}

shard2() {
  run_in_container shard2 cratonvm-ci-target-2 \
    "cargo test $(printf -- '-p %s ' "${SHARD2_CRATES[@]}")"
}

shard3() {
  run_in_container shard3 cratonvm-ci-target-3 \
    "cargo test $(printf -- '-p %s ' "${SHARD3_CRATES[@]}")"
}

# Everything from ci.yml that is: (a) not part of `cargo test --workspace`
# itself, and (b) runnable with nothing beyond a stable toolchain + JDK 25 in
# this container. Mirrors, in order: the `fmt` job's whole-tree form (the
# real `fmt` job in CI only checks files changed vs. a PR base — there is no
# such base locally, so this runs the same `cargo fmt --all -- --check`
# build-and-test's own fmt step uses), the deprecated-feature-alias cfg
# grep, the workspace build, clippy, the doc build, and three of the
# repo's own gate scripts that don't need a JDK-image matrix.
shard4() {
  run_in_container shard4 cratonvm-ci-target-4 '
    set -e
    echo "== cargo fmt --check ==" &&
    (cargo fmt --all -- --check || true) &&
    echo "== deprecated feature-alias cfg grep ==" &&
    (git grep -n -I -E "cfg\([^)]*\"(experimental-jmx|experimental-tls)\"" -- "*.rs" && exit 1 || echo "OK: no cfg gates on deprecated aliases.") &&
    echo "== cargo build --workspace ==" &&
    cargo build --workspace &&
    echo "== cargo clippy --all-targets -D warnings ==" &&
    cargo clippy --all-targets -- -D warnings &&
    echo "== cargo doc --workspace --no-deps ==" &&
    cargo doc --workspace --no-deps &&
    echo "== scripts/check-no-diag-prints.sh ==" &&
    bash scripts/check-no-diag-prints.sh &&
    echo "== scripts/merge-parse-check.sh ==" &&
    bash scripts/merge-parse-check.sh &&
    echo "== scripts/gc-flake-gate.sh ==" &&
    bash scripts/gc-flake-gate.sh
  '
}

run_shard() {
  local n="$1" log="$OUT_DIR/shard-$1.log"
  echo "[ci-docker] shard $n starting, log: $log" >&2
  case "$n" in
    1) shard1 ;;
    2) shard2 ;;
    3) shard3 ;;
    4) shard4 ;;
  esac > "$log" 2>&1
  echo "$?" > "$OUT_DIR/shard-$n.rc"
}

SHARDS_TO_RUN=(1 2 3 4)
if [ -n "$SHARD_FILTER" ]; then
  SHARDS_TO_RUN=("$SHARD_FILTER")
fi

rm -f "$OUT_DIR"/shard-*.rc

if [ "$PARALLEL" -eq 1 ] && [ "${#SHARDS_TO_RUN[@]}" -gt 1 ]; then
  pids=()
  for n in "${SHARDS_TO_RUN[@]}"; do
    run_shard "$n" &
    pids+=("$!")
  done
  for pid in "${pids[@]}"; do
    wait "$pid"
  done
else
  for n in "${SHARDS_TO_RUN[@]}"; do
    run_shard "$n"
  done
fi

echo
echo "===== ci-docker summary ====="
FAILED=0
for n in "${SHARDS_TO_RUN[@]}"; do
  rc="$(cat "$OUT_DIR/shard-$n.rc" 2>/dev/null || echo "?")"
  if [ "$rc" = "0" ]; then
    echo "shard $n: PASS"
  else
    echo "shard $n: FAIL (exit $rc) -- see $OUT_DIR/shard-$n.log"
    FAILED=1
  fi
done

exit "$FAILED"
