#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# jdk-only-inherited-decl.sh — re-read the census's "class present, method not
# declared" bucket with the class hierarchy in hand.
#
# WHY THIS EXISTS
#
# `image_declaring_method` asks the image about ONE class name. A native
# registered on `sun/nio/ch/SocketDispatcher.close(Ljava/io/FileDescriptor;)V`
# comes back `declared: false`, and every reader of that column — including
# `scripts/jdk-only-adjudicate.py`, whose table calls the bucket "dead or
# misdescribed" — takes it to mean the registration targets nothing.
#
# For three quarters of the bucket that is wrong. `close` is concrete bytecode
# on `sun.nio.ch.UnixDispatcher`, two frames up, and CratonVM's receiver-driven
# dispatch finds the registration first: the row is a contract §1.4 SHADOW of
# inherited bytecode, dispatched in anger, not a dead entry. Measured on JDK 25
# (2026-08-05): of 2,542 `declared: false` rows, **1,939 are inherited** (1,612
# concrete, 308 abstract, 19 ACC_NATIVE) and only 603 are genuinely nowhere.
#
# The 19 ACC_NATIVE ones matter most: those are §1.5 bridges the census failed
# to credit, because the declaration is on a supertype.
#
# Usage:
#   sh scripts/jdk-only-inherited-decl.sh <census.json> [out.tsv]
#
# Environment:
#   JAVA_HOME   real JDK runtime image — MUST be the same image the census was
#               taken against, or the answer describes a different JDK
#   PYTHON      python interpreter (default: python3)
#
# Exit codes:
#   0  the report was produced
#   3  a prerequisite is missing (no census, no JDK, no python)

set -eu

CENSUS="${1:-}"
OUT="${2:-}"
if [ -z "$CENSUS" ] || [ ! -s "$CENSUS" ]; then
    echo "usage: sh scripts/jdk-only-inherited-decl.sh <census.json> [out.tsv]" >&2
    echo "       (take one with --dump-native-registry --explain-jdk-only)" >&2
    exit 3
fi

ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null || true)"
[ -n "$ROOT" ] || ROOT="$(cd "$(dirname "$0")/.." && pwd)"

PY="${PYTHON:-python3}"
command -v "$PY" >/dev/null 2>&1 || { echo "ERROR: no $PY on PATH." >&2; exit 3; }

JDK="${JAVA_HOME:-}"
if [ -z "$JDK" ] || [ ! -x "$JDK/bin/java" ]; then
    echo "ERROR: JAVA_HOME must point at the SAME JDK image the census used." >&2
    echo "       A hierarchy answer from a different image is not an answer." >&2
    exit 3
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
TRIPLES="$WORK/undecl.tsv"
[ -n "$OUT" ] || OUT="$WORK/undecl-out.tsv"

# Every row the census calls "class present, method not declared".
"$PY" - "$CENSUS" "$TRIPLES" <<'EOF'
import json, sys
census, out = sys.argv[1], sys.argv[2]
with open(census, encoding="utf-8") as fh:
    doc = json.load(fh)
if not doc.get("image_adjudication"):
    sys.exit("REFUSING: census has image_adjudication false -- rerun the VM "
             "with --explain-jdk-only, or every row here is null for the wrong "
             "reason.")
n = 0
with open(out, "w", encoding="utf-8") as fh:
    for r in doc["natives"]:
        img = r.get("image_declaring_method") or {}
        if img.get("image_has_class") and not img.get("declared"):
            fh.write("%s\t%s\t%s\n" % (r["class"], r["name"], r["descriptor"]))
            n += 1
print("undecl rows: %d" % n)
EOF

"$JDK/bin/javac" -d "$WORK" "$ROOT/apps/probes/InheritedDeclProbe.java"
"$JDK/bin/java" -cp "$WORK" InheritedDeclProbe "$TRIPLES" "$OUT"

"$PY" - "$OUT" <<'EOF'
import sys
from collections import Counter
by = Counter()
nat = []
for line in open(sys.argv[1], encoding="utf-8"):
    f = line.rstrip("\n").split("\t")
    if len(f) < 6:
        continue
    by[(f[3], f[5].split(",")[0] if f[5] != "-" else "-")] += 1
    if f[3] == "INHERITED" and "native" in f[5]:
        nat.append(f)
print("\n=== how the 'not declared' bucket actually resolves ===")
for (verdict, mods), n in sorted(by.items(), key=lambda kv: -kv[1]):
    print("  %-12s %-10s %6d" % (verdict, mods, n))
print("\n=== INHERITED as ACC_NATIVE: §1.5 bridges the census did not credit ===")
for f in nat:
    print("  %s.%s%s  ->  %s" % (f[0], f[1], f[2], f[4]))
print("  total %d" % len(nat))
EOF

[ "$OUT" = "$WORK/undecl-out.tsv" ] || echo "\nper-row TSV written to $OUT"
