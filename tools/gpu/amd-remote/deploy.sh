#!/usr/bin/env bash
# Copy this tree to a remote Linux AMD box and run the bring-up.
#
#   bash tools/gpu/amd-remote/deploy.sh <host> <ssh-key> [user] [--quick]
#
# e.g.
#   bash tools/gpu/amd-remote/deploy.sh 203.0.113.7 ~/.ssh/amd.pem ubuntu
#   bash tools/gpu/amd-remote/deploy.sh 203.0.113.7 ~/.ssh/amd.pem ubuntu --quick
#
# Does three things and stops: copy, run, fetch the report back. The
# interesting logic is all in `bringup.sh`, which runs on the far side.
#
# ── WHAT IS COPIED, AND WHAT IS NOT ───────────────────────────────────
#
# The working tree minus everything that is large, machine-specific or
# regenerable: `target*/`, `.git/`, the committed `.class` fixtures
# (recompiled there from the `.java` sources beside them), and the
# benchmark result dumps. On a normal connection that is a couple of
# minutes rather than half an hour, which matters when GPU time is the
# scarce thing.
#
# `rsync` if it is available, `scp` of a tarball if it is not — a fresh
# cloud image often has neither rsync nor a compiler, and the point of
# this script is to work on the box you were given, not the box you
# wanted.

set -euo pipefail

HOST="${1:-}"
KEY="${2:-}"
USER_NAME="${3:-}"
QUICK="${4:-}"

if [ -z "$HOST" ] || [ -z "$KEY" ]; then
  cat >&2 <<'USAGE'
usage: deploy.sh <host> <ssh-key> [user] [--quick]

  host      IP or hostname of the Linux box with the AMD GPU
  ssh-key   path to the private key
  user      remote user (default: root, then ubuntu, then the local name)
  --quick   stop after the first real kernel launch
USAGE
  exit 2
fi

# `--quick` may arrive in the user slot when the user is omitted.
if [ "$USER_NAME" = "--quick" ]; then
  QUICK="--quick"
  USER_NAME=""
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
REMOTE_DIR="${REMOTE_DIR:-~/cratonvm}"
SSH_OPTS=(-i "$KEY" -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15)

# ── Pick a user that can actually log in ──────────────────────────────
pick_user() {
  if [ -n "$USER_NAME" ]; then
    echo "$USER_NAME"
    return
  fi
  for candidate in root ubuntu ec2-user admin "$(whoami)"; do
    if ssh "${SSH_OPTS[@]}" -o BatchMode=yes "${candidate}@${HOST}" true 2>/dev/null; then
      echo "$candidate"
      return
    fi
  done
  echo "root"
}

REMOTE_USER="$(pick_user)"
TARGET="${REMOTE_USER}@${HOST}"
echo "==> remote: $TARGET:$REMOTE_DIR"

echo "==> checking the connection"
ssh "${SSH_OPTS[@]}" "$TARGET" 'uname -a; echo "---"; (command -v hipconfig >/dev/null && hipconfig --version | head -2) || echo "hipconfig: not found"; (command -v cargo >/dev/null && cargo --version) || echo "cargo: NOT FOUND — install rustup first"'

# Excluded because it is large, machine-specific or regenerable. The
# `.class` files are excluded on purpose: they are rebuilt from the
# `.java` sources beside them, so a stale copy cannot silently be the
# thing that runs.
EXCLUDES=(
  --exclude '.git'
  --exclude 'target'
  --exclude 'target-*'
  --exclude '*.class'
  --exclude 'tools/bench/bench-gpu/results/dumps'
  --exclude 'hip-dump'
  --exclude '*.bin'
)

echo "==> copying the tree"
if command -v rsync >/dev/null 2>&1; then
  rsync -az --delete "${EXCLUDES[@]}" \
    -e "ssh ${SSH_OPTS[*]}" \
    "$REPO_ROOT/" "$TARGET:$REMOTE_DIR/"
else
  echo "    (no rsync; falling back to a tarball over scp)"
  TAR="$(mktemp -t cratonvm-XXXXXX.tar.gz)"
  tar -czf "$TAR" -C "$REPO_ROOT" \
    --exclude='.git' --exclude='target' --exclude='target-*' \
    --exclude='*.class' --exclude='hip-dump' --exclude='*.bin' .
  ssh "${SSH_OPTS[@]}" "$TARGET" "mkdir -p $REMOTE_DIR"
  scp "${SSH_OPTS[@]}" "$TAR" "$TARGET:$REMOTE_DIR/tree.tar.gz"
  ssh "${SSH_OPTS[@]}" "$TARGET" "cd $REMOTE_DIR && tar -xzf tree.tar.gz && rm tree.tar.gz"
  rm -f "$TAR"
fi

echo "==> running the bring-up"
# `-t` so the far side's progress is visible live rather than arriving
# in one block at the end; GPU time is the scarce resource and watching
# stage 3 fail early is worth more than a tidy log.
ssh -t "${SSH_OPTS[@]}" "$TARGET" \
  "cd $REMOTE_DIR && REPORT=amd-bringup.md bash tools/gpu/amd-remote/bringup.sh ${QUICK}" \
  || echo "==> bring-up reported failures (see the report)"

echo "==> fetching the report"
scp "${SSH_OPTS[@]}" "$TARGET:$REMOTE_DIR/amd-bringup.md" . 2>/dev/null \
  && echo "==> wrote ./amd-bringup.md" \
  || echo "==> no report to fetch"

# The rendered kernels are the artefact worth reading when a number
# looks wrong, so they come back too.
scp -r "${SSH_OPTS[@]}" "$TARGET:$REMOTE_DIR/hip-dump" . 2>/dev/null \
  && echo "==> wrote ./hip-dump/ (rendered HIP C++)" \
  || true
