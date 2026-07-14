#!/usr/bin/env bash
set -euo pipefail
repo=/data/data/cratonvm-hib-longtail-residuals-20260713-azure-002
cd "$repo"
sed -i '/#\[cfg(target_os = "windows")\]/,+3c\    const JIT_ABI_MAX_JAVA_ARGS: usize = 8;' vm/src/runtime/interpreter.rs
sed -i 's/JIT_ABI_REG_SLOTS/JIT_ABI_MAX_JAVA_ARGS/g' vm/src/runtime/interpreter.rs
rm -f vm/src/runtime/interpreter.rs.rej
sed -i 's/\r$//' vm/src/runtime/interpreter.rs
/home/victor/.cargo/bin/rustfmt vm/src/runtime/interpreter.rs
git diff --check
git status --short
grep -n JIT_ABI_MAX_JAVA_ARGS vm/src/runtime/interpreter.rs
