#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Rank the `--jdk-only` shadows by what COMPILED code pays for them.

Proposal SH16-1 of `docs/internal/jit-proposals/jit-r16-shadow-proposals-RETIRED-20261006.md`
(implemented round 16 wave 15, lane shadow). The static census
(`scripts/baselines/jdk-only-kind-map-25-linux.tsv`) says which registrations
shadow bytecode; it cannot say which of them a workload's hot code meets. The
JIT can: with `CRATONVM_DBG_JITC=1` every inline site it refuses because a
registered native sits over the callee prints

    [cratonvm-jitc] inline-resolve REFUSED <class>.<method><descriptor> depth=<n>: native-shadow

This script collects those lines from one or more logs (or stdin), joins each
triple to the kind map, and prints the rows ranked by refusal count.

Usage (bash, repo root):

    CRATONVM_DBG_JITC=1 cratonvm ... -cp probes P1 2> p1.log
    python3 tools/scripts/jit-native-shadow-rank.py p1.log p2.log ...
    python3 tools/scripts/jit-native-shadow-rank.py --all-kinds p1.log

Columns: refusals, how many of them were nested (depth > 0), kind (the LAST
registration of the triple in the kind map, since registration is
last-write-wins; `?` = not in the map), `retired` when
`native-api/src/retired_shadow.rs` already lists the triple (a refusal on a
retired row means the binary predates the table, or
`CRATONVM_UNRETIRE_NATIVE_SHADOW` is armed), then the triple.

By default only `bridge` rows (and `?` rows) are printed: an `intrinsic` row is
a reviewed decision and a `synthetic-stub` row is dropped under `--jdk-only`,
so neither is a retirement candidate. The kind map does not adjudicate `Code`:
an `ACC_NATIVE` target (`Object.getClass`, `System.arraycopy`, ...) is listed
like any other `bridge` row. Pass `--acc-native FILE` (one
`class<TAB>method<TAB>descriptor` per line, e.g. cut from a schema-3 census)
to drop them.

Exit status: 0 with a table, 2 when no `native-shadow` refusal was found (a
log taken without `CRATONVM_DBG_JITC=1` reads exactly like a clean one, so an
empty result is refused rather than printed as a table of nothing).
"""

import argparse
import collections
import os
import re
import sys

LINE = re.compile(
    r"inline-resolve REFUSED (?P<target>\S+) depth=(?P<depth>\d+): (?P<why>native-shadow\S*)"
)
TUPLE = re.compile(r'\(\s*"([^"]+)",\s*"([^"]+)",\s*"([^"]+)",?\s*\)')
TABLE = re.compile(r"static (RETIRED_SHADOW_\w+): &\[\(&str, &str, &str\)\] =\s*&\[")


def split_target(target):
    """`java/lang/Object.equals(Ljava/lang/Object;)Z` -> (class, method, descriptor).

    The descriptor starts at the first `(`; a JVM internal class name never
    contains `.`, so the last `.` before the `(` splits class from method.
    """
    paren = target.find("(")
    if paren < 0:
        return None
    head, desc = target[:paren], target[paren:]
    dot = head.rfind(".")
    if dot <= 0:
        return None
    return head[:dot], head[dot + 1 :], desc


def load_kind_map(path):
    """(class, method, descriptor) -> kind of the highest ordinal."""
    kinds = {}
    best = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.startswith("#"):
                continue
            fields = line.rstrip("\n").split("\t")
            if len(fields) < 5:
                continue
            key = (fields[0], fields[1], fields[2])
            try:
                ordinal = int(fields[3])
            except ValueError:
                continue
            if key not in best or ordinal >= best[key]:
                best[key] = ordinal
                kinds[key] = fields[4]
    return kinds


def load_retired(path):
    """Every triple of every table `RETIRED_SHADOW_TABLES` lists."""
    if not os.path.isfile(path):
        return set()
    with open(path, encoding="utf-8") as f:
        src = f.read()
    start = src.find("pub(crate) const RETIRED_SHADOW_TABLES")
    listed = src[start : src.find("];", start)] if start >= 0 else ""
    out = set()
    for m in TABLE.finditer(src):
        if not re.search(r"\b" + m.group(1) + r",", listed):
            continue
        end = src.find("];", m.end())
        out.update(TUPLE.findall(src[m.end() : end]))
    return out


def load_triples(path):
    out = set()
    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.startswith("#"):
                continue
            fields = line.rstrip("\n").split("\t")
            if len(fields) >= 3:
                out.add((fields[0], fields[1], fields[2]))
    return out


def collect(lines, counts, nested):
    for line in lines:
        m = LINE.search(line)
        if not m:
            continue
        triple = split_target(m.group("target"))
        if triple is None:
            continue
        counts[triple] += 1
        if m.group("depth") != "0":
            nested[triple] += 1


def main(argv):
    here = os.path.dirname(os.path.abspath(__file__))
    repo = os.path.dirname(os.path.dirname(here))
    ap = argparse.ArgumentParser(
        description="Rank --jdk-only native shadows by CRATONVM_DBG_JITC=1 refusals."
    )
    ap.add_argument("logs", nargs="*", help="CRATONVM_DBG_JITC=1 logs (default: stdin)")
    ap.add_argument(
        "--kind-map",
        default=os.path.join(repo, "scripts", "baselines", "jdk-only-kind-map-25-linux.tsv"),
    )
    ap.add_argument(
        "--retired",
        default=os.path.join(repo, "native-api", "src", "retired_shadow.rs"),
    )
    ap.add_argument("--acc-native", help="TSV of ACC_NATIVE triples to drop")
    ap.add_argument("--all-kinds", action="store_true", help="print intrinsic and stub rows too")
    ap.add_argument("--top", type=int, default=0, help="print only the first N rows")
    args = ap.parse_args(argv)

    counts = collections.Counter()
    nested = collections.Counter()
    if args.logs:
        for path in args.logs:
            with open(path, encoding="utf-8", errors="replace") as f:
                collect(f, counts, nested)
    else:
        collect(sys.stdin, counts, nested)
    if not counts:
        print(
            "no `inline-resolve REFUSED ... native-shadow` line in the input: was "
            "CRATONVM_DBG_JITC=1 set, and did anything compile?",
            file=sys.stderr,
        )
        return 2

    kinds = load_kind_map(args.kind_map)
    retired = load_retired(args.retired)
    acc_native = load_triples(args.acc_native) if args.acc_native else set()

    rows = []
    for triple, n in counts.items():
        kind = kinds.get(triple, "?")
        if triple in acc_native:
            continue
        if not args.all_kinds and kind not in ("bridge", "?"):
            continue
        rows.append((n, kind, triple))
    rows.sort(key=lambda r: (-r[0], r[2]))
    if args.top:
        rows = rows[: args.top]

    print(
        f"# {sum(counts.values())} native-shadow refusals over {len(counts)} triples; "
        f"{len(rows)} printed"
    )
    print("refusals\tnested\tkind\tretired\ttriple")
    for n, kind, (c, m, d) in rows:
        mark = "retired" if (c, m, d) in retired else ""
        print(f"{n}\t{nested[(c, m, d)]}\t{kind}\t{mark}\t{c}.{m}{d}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
