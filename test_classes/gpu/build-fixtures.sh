#!/usr/bin/env bash
# Compile the GPU `.class` fixtures the Rust tests load at run time.
#
#   bash test_classes/gpu/build-fixtures.sh
#
# ## Why this is a step at all
#
# `.gitignore` ignores `test_classes/**/*.class` on purpose: the `.java`
# here is the source of truth and the `.class` is a build artefact. That
# is a fine arrangement, but it means a fresh checkout has none of them,
# and `vm/tests/gpu_offload_features.rs` loads them through a real
# classpath. Its four device tests are `#[ignore]`d, so nobody meets the
# problem until they do the one thing the module docs tell them to do —
# run the ignored tests on a GPU box — and get
# `ClassNotFound { class_name: "EligibleVectorAdd" }`, which reads like a
# broken class loader rather than a missing build step. It said nothing
# about a fixture needing to be compiled first, so this script exists and
# those docs now name it.
#
# ## The two that need the jar
#
# `BenchmarkExplicit` and `SpontaneousCompletionCheck` import
# `craton.gpu.*`. That jar is produced by `craton-gpu/build.rs`, so it
# only exists after a cargo build. If it cannot be found, this compiles
# the other 42 and says which two it skipped and why — a script that
# silently produced 42 of 44 fixtures would be the same trap this exists
# to remove.
#
# Env:
#   JDK               javac to use (default: the TornadoVM JDK 25 this
#                     repo is validated against)
#   CRATON_GPU_JAR    path to craton-gpu-annotations.jar; auto-discovered
#                     under target*/ when unset
set -eu

cd "$(dirname "$0")/../.."          # workspace root
JDK="${JDK:-C:/craton/TornadoVM/jdk-25.0.3}"
JAVAC="$JDK/bin/javac"
OUT=test_classes/gpu

if [ ! -x "$JAVAC" ] && [ ! -f "$JAVAC.exe" ]; then
  echo "no javac at $JAVAC — set JDK to a JDK 21+ install" >&2
  exit 1
fi

# Newest jar wins: a stale one from an older target dir would compile
# against an API that no longer matches the VM under test.
if [ -z "${CRATON_GPU_JAR:-}" ]; then
  CRATON_GPU_JAR=$(ls -t target*/release/build/cratonvm-gpu-*/out/craton-gpu-annotations.jar \
                          target*/debug/build/cratonvm-gpu-*/out/craton-gpu-annotations.jar \
                     2>/dev/null | head -1 || true)
fi

needs_jar=$(grep -lE '^import craton\.gpu\.' "$OUT"/*.java || true)
plain=$(ls "$OUT"/*.java | grep -vxF "$needs_jar" || ls "$OUT"/*.java)

# shellcheck disable=SC2086
"$JAVAC" -nowarn -d "$OUT" $plain
echo "compiled $(echo "$plain" | wc -l) fixtures into $OUT"

if [ -z "$needs_jar" ]; then
  exit 0
fi
if [ -n "${CRATON_GPU_JAR:-}" ] && [ -f "$CRATON_GPU_JAR" ]; then
  # shellcheck disable=SC2086
  "$JAVAC" -nowarn -cp "$CRATON_GPU_JAR" -d "$OUT" $needs_jar
  echo "compiled $(echo "$needs_jar" | wc -l) more against $CRATON_GPU_JAR"
else
  echo
  echo "SKIPPED (no craton-gp[u]-annotations.jar found):"
  echo "$needs_jar" | sed 's/^/  /'
  echo "These import craton.gpu.*. The jar is built by craton-gpu/build.rs, so"
  echo "run any cargo build first, or set CRATON_GPU_JAR, then re-run this."
  echo "Nothing in vm/tests/ needs them today; bench-gpu drives them."
fi
