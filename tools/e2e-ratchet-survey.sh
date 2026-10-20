#!/usr/bin/env bash
# Which vm/tests targets actually RUN when their prerequisites are present?
#
# `vm/tests/common/mod.rs` turns a missing `cratonvm` binary, JDK or Java
# fixture into an `eprintln!` + `return`, at which point cargo prints
# `test ... ok`. 95 of 174 files in vm/tests use that helper and only three
# are on ci.yml's `CRATONVM_REQUIRE_E2E` list, so the rest are green for a
# reason nobody has checked.
#
# This checks. Every `common`-using target is run twice -- once plain, once
# with CRATONVM_REQUIRE_E2E=1 -- and classified:
#
#   RUNS      passes with prerequisites demanded. A ratchet candidate: it
#             was earning its green.
#   VACUOUS   passes plain, fails only under REQUIRE_E2E. Its green was the
#             skip, not the assertions.
#   RED       fails either way. A real failure, not a prerequisite question.
#
# ci.yml's rule is that a target joins the list only after someone has run
# it against a real binary. This is that run.
#
#   CRATONVM_BIN=/path/to/cratonvm.exe JAVA_HOME=/path/to/jdk \
#     bash tools/e2e-ratchet-survey.sh
set -u
: "${CRATONVM_BIN:?set CRATONVM_BIN to a built cratonvm binary}"
: "${JAVA_HOME:?set JAVA_HOME to a real JDK}"
export CRATONVM_BIN JAVA_HOME
TD=${CARGO_TARGET_DIR:-target}
OUT=${OUT:-/tmp/e2e-survey}
mkdir -p "$OUT"

targets=$(grep -rl "mod common\|common::" vm/tests/*.rs | sed 's|.*/||;s|\.rs$||' | sort)
echo "surveying $(echo "$targets" | wc -l) targets"

for t in $targets; do
  plain=$(CARGO_TARGET_DIR="$TD" cargo test -p cratonvm-vm --test "$t" 2>&1 | tail -40)
  echo "$plain" > "$OUT/$t.plain"
  if ! echo "$plain" | grep -qE "^test result: ok"; then
    echo "RED      $t"
    continue
  fi
  req=$(CRATONVM_REQUIRE_E2E=1 CARGO_TARGET_DIR="$TD" cargo test -p cratonvm-vm --test "$t" 2>&1 | tail -40)
  echo "$req" > "$OUT/$t.require"
  if echo "$req" | grep -qE "^test result: ok"; then
    echo "RUNS     $t"
  else
    echo "VACUOUS  $t"
  fi
done
echo "SURVEY DONE"
