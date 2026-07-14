#!/usr/bin/env bash
set -euo pipefail
repo=/data/data/cratonvm-hib-longtail-residuals-20260713-azure-002
base_repo=/data/data/cratonvm
base_commit=d51fa6938385becacef6102e75d402879f1c67bb
for file in jit/src/x64.rs native-builtins/src/apps_h2.rs vm/src/jit/helpers.rs; do
  base=/tmp/hbl-base-$(basename "$file")
  local=/tmp/$(basename "$file")
  merged=/tmp/hbl-merged-$(basename "$file")
  git -C "$base_repo" show "$base_commit:$file" > "$base"
  git merge-file -p "$repo/$file" "$base" "$local" > "$merged"
  perl -pi -e 's/\r$//' "$merged"
  cp "$merged" "$repo/$file"
done
cd "$repo"
/home/victor/.cargo/bin/rustfmt jit/src/x64.rs native-builtins/src/apps_h2.rs vm/src/jit/helpers.rs
git diff --check
git status --short
