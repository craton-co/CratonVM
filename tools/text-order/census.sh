#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company
#
# tools/text-order/census.sh -- regenerate tools/text-order/boot-text.order
# (Linux x86_64, needs gcc, python3, binutils and a JDK 25). See README.md.
#
# Usage (from the workspace root):  JAVA_HOME=/path/to/jdk-25 tools/text-order/census.sh
#
# 1. Builds `cratonvm` with every function on its own 4 KiB page
#    (`-C llvm-args=-align-all-functions=12`; LTO off, 16 codegen units: the
#    template keys demangled names, so the census build's profile does not
#    have to match the release's) into $CENSUS_TARGET_DIR.
# 2. Runs the boot probes under texttrap.so: Zr7RsBoot hello, Zr7RsBoot
#    classes 3000, and a generated 300-method JIT probe. One executed page is
#    one executed function in this build.
# 3. Writes the template: the functions in first-execution order (hello's
#    order first, then what the later probes add).
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
here="$root/tools/text-order"
jdk="${JAVA_HOME:?set JAVA_HOME to a JDK 25}"
target_dir="${CENSUS_TARGET_DIR:-$root/target/text-order-census}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "census: function-per-page build into $target_dir"
(cd "$root" && CARGO_TARGET_DIR="$target_dir" CARGO_PROFILE_RELEASE_LTO=off \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
    RUSTFLAGS="${RUSTFLAGS:-} -C llvm-args=-align-all-functions=12" \
    cargo build --release -p cratonvm-cli --bin cratonvm)
bin="$target_dir/release/cratonvm"

gcc -O2 -shared -fPIC -o "$work/texttrap.so" "$here/texttrap.c" -ldl
mkdir -p "$work/p"
"$jdk/bin/javac" -d "$work/p" "$root/tools/probes/Zr7RsBoot.java"
"$jdk/bin/java" -cp "$work/p" Zr7RsBoot list > "$work/names.txt"
python3 - "$work/p/TextOrderJit.java" <<'EOF'
import sys
n = 300
src = ["public class TextOrderJit {"]
for i in range(n):
    src.append(f"  static long m{i}(long x) {{ long s = x; for (int k = 0; k < {8 + i % 7}; k++) "
               f"{{ s = s * {31 + i} + (k ^ {i}); if ((s & 7) == {i % 8}) s += {i}; }} return s; }}")
src.append("  public static void main(String[] a) { long s = 0; for (int r = 0; r < 4000; r++) {")
src += [f"      s += m{i}(r);" for i in range(n)]
src.append('  } System.out.println("textorderjit " + s); } }')
open(sys.argv[1], "w").write("\n".join(src) + "\n")
EOF
"$jdk/bin/javac" -d "$work/p" "$work/p/TextOrderJit.java"

traps=()
run() {
    local tag=$1; shift
    LD_PRELOAD="$work/texttrap.so" TEXTTRAP_OUT="$work/trap-$tag" "$bin" "$@" > "$work/$tag.out" 2>&1
    # The launcher re-executes itself: keep the VM process's (largest) file.
    traps+=("$(ls -S "$work"/trap-"$tag".* | head -1)")
    echo "census: $tag executed $(( $(stat -c %s "${traps[-1]}") / 8 - 1 )) pages"
}
run hello -Xmx256m -cp "$work/p" Zr7RsBoot hello 0
run cls -Xmx256m -cp "$work/p" Zr7RsBoot classes 3000 "$work/names.txt"
run jit -Xmx256m -cp "$work/p" TextOrderJit

python3 "$here/order.py" template "$bin" "$here/boot-text.order.new" "${traps[@]}"
{
    head -1 "$here/boot-text.order.new"
    echo "# generated $(date -u +%Y-%m-%d) at $(git -C "$root" rev-parse --short HEAD) by tools/text-order/census.sh (hello, classes 3000, 300-method JIT probe)"
    tail -n +2 "$here/boot-text.order.new"
} > "$here/boot-text.order"
rm -f "$here/boot-text.order.new"
echo "census: wrote $here/boot-text.order"
