#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# jdk-only-phase2-battery.sh — the question the corpus does not ask.
#
# ---------------------------------------------------------------------------
# Why the corpus is not enough
# ---------------------------------------------------------------------------
#
# `jdk_only_enforce_shadow`'s own doc states it: "the corpus does not ask about
# array component types, the CONTENT of a built string, or the class identity of
# a returned object." Measured, on 2026-08-30:
#
#   * the 14-vector smoke set passes `ConcurrentHashMap` armed, 14/14;
#   * `ChmShadowSweep`, the family's OWN content probe, is 0-diff over 39 357
#     yields;
#   * `MapViewsShadowSweep` — a DIFFERENT family's probe — dies at row 261 of
#     302, because JDK 25's `java.util.Properties` delegates to an internal
#     `ConcurrentHashMap`, so every `Properties` view empties out.
#
# **A retirement's blast radius is its class's USERS**, and the family's own
# probe being clean is the trap rather than the reassurance. So this runs the
# WHOLE probe tree, not the family's.
#
# ---------------------------------------------------------------------------
# THREE columns, because two cannot say which way a row moved
# ---------------------------------------------------------------------------
#
#   hs      HotSpot, the oracle
#   base    --jdk-only, unarmed
#   armed   --jdk-only, the candidate scope armed
#
# `changed` (base vs armed) says a row MOVED and cannot say whether that is
# better or worse — and the direction is not always bad: armed, `ArrayDeque`
# starts throwing `ConcurrentModificationException` on modification during
# iteration, which is what HotSpot does and what the native never did.
#
# The verdict column is therefore **delta = d(hs,armed) - d(hs,base)**:
# negative moves the VM TOWARD the oracle, and only positive is a reason not to
# retire.
#
# ---------------------------------------------------------------------------
# Two traps this driver prints its way out of
# ---------------------------------------------------------------------------
#
#   * **A dial that was never asked makes every zero meaningless.** `y/r` is
#     `enforcement_dial` `yielded/reached`; `0/0` is marked VACUOUS rather than
#     allowed to read as a pass.
#   * **A probe that dies early prints FEWER lines, and `diff` reports the
#     missing tail as ordinary rows.** The line counts sit beside the diff, and
#     a mismatch is called out, so a crash cannot be read as 40 wrong answers.
#
# Baselines are taken by THIS run. Reusing a `.base` from an earlier binary is
# how `DequeListShadowSweep`'s unrelated 2 -> 0 nearly got credited to a
# retirement — a lead measured against yesterday's binary is a lead about
# yesterday's binary.
#
# ---------------------------------------------------------------------------
# Usage
# ---------------------------------------------------------------------------
#
#   CV=/path/to/cratonvm JDK=/path/to/jdk WORKTREE=/path/to/repo \
#   SCOPE="java/util/concurrent/ConcurrentHashMap" OUT=battery.log \
#     scripts/jdk-only-phase2-battery.sh
#
# SCOPE is a `CRATONVM_ENFORCE_NATIVE_SHADOW` value: one prefix, a
# comma-separated list, or `all`.
set -u

: "${CV:?set CV to the cratonvm binary under test}"
: "${JDK:?set JDK to the java-home to diff against}"
: "${WORKTREE:?set WORKTREE to the repo root holding apps/probes/}"
: "${SCOPE:?set SCOPE to the CRATONVM_ENFORCE_NATIVE_SHADOW value under test}"
: "${OUT:=jdk-only-phase2-battery.log}"
: "${TIMEOUT:=600}"

cd "$WORKTREE" || exit 1
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

O=$(mktemp -d "${TMPDIR:-/tmp}/p2bat.XXXXXX")
trap 'rm -rf "$O"' EXIT

: > "$OUT"
printf '=== %s vm=%s scope_bytes=%s\n' "$(date -Is)" "$(sha256sum "$CV" | cut -c1-16)" "${#SCOPE}" >> "$OUT"

ALL=$(ls apps/probes/*.java 2>/dev/null | xargs -n1 basename | sed 's/\.java$//')
[ -n "$ALL" ] || { echo "no probes under apps/probes/" >> "$OUT"; exit 1; }

# Compile first and SAY which ones failed. A battery that skips on a compile
# error and prints one line about it is a battery whose coverage has to be read
# from its own log: the original run silently omitted `JdkInternalSweep`, which
# needs --add-exports, and reported "80 probes" as though that were the tree.
skipped=0
for C in $ALL; do
  if ! "$JDK/bin/javac" -d apps/probes/out "apps/probes/$C.java" >/dev/null 2>&1; then
    printf '%-26s JAVAC-FAILED (excluded from the counts below)\n' "$C" >> "$OUT"
    skipped=$((skipped + 1))
  fi
done

measured=0 worse=0 better=0 vacuous=0 truncated=0
for C in $ALL; do
  [ -f "apps/probes/out/$C.class" ] || continue
  measured=$((measured + 1))

  timeout "$TIMEOUT" "$JDK/bin/java" -cp apps/probes/out "$C" > "$O/hs" 2>/dev/null
  timeout "$TIMEOUT" "$CV" --java-home "$JDK" --jdk-only -cp apps/probes/out "$C" > "$O/base" 2>/dev/null
  b=$?
  timeout "$TIMEOUT" env CRATONVM_ENFORCE_NATIVE_SHADOW="$SCOPE" \
      "$CV" --java-home "$JDK" --jdk-only --jdk-only-report "$O/rep.json" \
      -cp apps/probes/out "$C" > "$O/armed" 2>/dev/null
  a=$?

  nb=$(grep -c . "$O/base"); na=$(grep -c . "$O/armed")
  db=$(diff "$O/hs" "$O/base"  | grep -c '^[<>]')
  da=$(diff "$O/hs" "$O/armed" | grep -c '^[<>]')
  delta=$((da - db))

  ry=$(python3 -c "
import json
try:
    e = json.load(open('$O/rep.json')).get('enforcement_dial') or {}
except Exception:
    e = {}
print('%d/%d' % (e.get('yielded') or 0, e.get('reached') or 0))" 2>/dev/null)

  note=""
  [ "$ry" = "0/0" ] && { note="$note   VACUOUS: the dial was never asked"; vacuous=$((vacuous + 1)); }
  [ "$nb" != "$na" ] && { note="$note   *** LINE COUNT MOVED $nb->$na: one side did not finish"; truncated=$((truncated + 1)); }
  [ "$delta" -gt 0 ] && worse=$((worse + 1))
  [ "$delta" -lt 0 ] && better=$((better + 1))

  printf '%-26s base=%-6d/%s armed=%-6d/%s d(hs,base)=%-4s d(hs,armed)=%-4s delta=%-5s y/r=%s%s\n' \
    "$C" "$nb" "$b" "$na" "$a" "$db" "$da" "$delta" "$ry" "$note" >> "$OUT"
done

{
  printf -- '--- measured %d, javac-skipped %d\n' "$measured" "$skipped"
  printf -- '    moved AWAY from HotSpot (delta>0): %d\n' "$worse"
  printf -- '    moved TOWARD HotSpot (delta<0):    %d\n' "$better"
  printf -- '    one side did not finish:           %d\n' "$truncated"
  printf -- '    vacuous (dial never asked):        %d\n' "$vacuous"
  printf 'DONE %s\n' "$(date -Is)"
} >> "$OUT"
