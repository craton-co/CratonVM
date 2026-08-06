#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Read a schema-3 native census and print the adjudication item 1 needs.

Usage:
    cratonvm --real-jdk --java-home <JDK> --explain-jdk-only \\
        --dump-native-registry census.json -cp probes JdkOnlyCensusLoadProbe
    python3 scripts/jdk-only-adjudicate.py census.json
    python3 scripts/jdk-only-adjudicate.py census.json --json block.json

`--explain-jdk-only` is not optional: without it `image_adjudication` is
false, every `image_declaring_method` is null, and this script refuses
rather than printing a table of zeroes that reads exactly like a clean
result.

The questions, in the order the record asks them:

  1. How many registrations state their kind vs inherit it (`kind_stated`)?
  2. Of the `Bridge` rows -- the dangerous direction, since Bridge is never the
     default -- how many target a method the class-path image declares as
     ACC_NATIVE?  Contract 1.5 defines a Bridge as what an ACC_NATIVE method
     binds to, so `acc_native: false` on a Bridge is a registration nobody
     adjudicated.
  3. How many natives of ANY kind shadow concrete bytecode (`has_code`)?

The `undecl` column below is a SUPERSET of "dead registration", and reading it
as one is a mistake this script cannot detect on its own.  `image_declaring_method`
asks the image about ONE class: a native registered on
`sun/nio/ch/SocketDispatcher.close` comes back `declared: false` while the
method is concrete bytecode on `sun.nio.ch.UnixDispatcher` two frames up.
Measured on JDK 25 (2026-08-05): **1,939 of 2,542 `undecl` rows are inherited**
-- 1,612 concrete shadows, 308 abstract, and 19 ACC_NATIVE bridges this table
does not credit.  Only 603 are dead.  Split them with

    sh scripts/jdk-only-inherited-decl.sh <census.json>

and pass its TSV back here as `--inherited <out.tsv>` to have section 2 broken
out rather than lumped.  Likewise `absent` is a superset: a class missing from
a Linux image may be the correct registration for Windows -- see
`scripts/jdk-only-platform-diff.py`.
  4. Which source file each unadjudicated group comes from, so the
     reclassification wave can be cut into subsystem-sized batches.

Section 7 is the same answer to question 2, as a **machine-readable block**,
and `--json FILE` writes it on its own.  It is not computed here: it comes from
`scripts/jdk-only-bridge-ratchet.py`, which is the L6 gate.  Two independent
implementations of "how many Bridge rows are unadjudicated" would drift, and
the one that drifted would be the one nobody was running.
"""
import importlib.util
import json
import os
import sys
from collections import Counter, defaultdict

_GATE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "jdk-only-bridge-ratchet.py")
_spec = importlib.util.spec_from_file_location("jdk_only_bridge_ratchet", _GATE)
ratchet = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ratchet)

INHERITED_TSV = None
argv = sys.argv[1:]
if "--inherited" in argv:
    i = argv.index("--inherited")
    if i + 1 >= len(argv):
        sys.exit("--inherited needs the TSV written by "
                 "scripts/jdk-only-inherited-decl.sh")
    INHERITED_TSV = argv[i + 1]
    del argv[i:i + 2]
json_out = None
if "--json" in argv:
    i = argv.index("--json")
    try:
        json_out = argv[i + 1]
    except IndexError:
        sys.exit("--json needs a path ('-' for stdout)")
    del argv[i:i + 2]
if not argv:
    sys.exit(__doc__)
path = argv[0]

with open(path, encoding="utf-8") as fh:
    doc = json.load(fh)

rows = doc["natives"]
print(f"file             {path}")
print(f"schema_version   {doc.get('schema_version')}")
print(f"mode             {doc.get('mode')}")
print(f"image_adjudication {doc.get('image_adjudication')}")
print(f"partial          {doc.get('partial', False)}")
print(f"rows             {len(rows)}")
print(f"counts           {doc.get('counts')}")
print(f"invocations      {doc.get('invocations')}")

if not doc.get("image_adjudication"):
    sys.exit("REFUSING to adjudicate: image_adjudication is false -- rerun with "
             "--explain-jdk-only. Every image_declaring_method is null because "
             "the pass did not run, not because the image lacks the method.")


def img(r):
    return r.get("image_declaring_method") or {}


print("\n=== 1. kind x kind_stated (registrations) ===")
c = Counter((r["kind"], r["kind_stated"]) for r in rows)
for kind in ("intrinsic", "bridge", "synthetic-stub"):
    stated, inherited = c[(kind, True)], c[(kind, False)]
    print(f"  {kind:<16} stated={stated:<6} inherited={inherited:<6} total={stated + inherited}")

print("\n=== 2. kind x what the IMAGE says ===")
print(f"  {'kind':<16}{'rows':>7}{'absent':>8}{'undecl':>8}{'native':>8}{'code':>8}{'abstract':>10}")
for kind in ("intrinsic", "bridge", "synthetic-stub"):
    sel = [r for r in rows if r["kind"] == kind]
    absent = sum(1 for r in sel if not img(r).get("image_has_class"))
    undecl = sum(1 for r in sel if img(r).get("image_has_class") and not img(r).get("declared"))
    nat = sum(1 for r in sel if img(r).get("acc_native"))
    code = sum(1 for r in sel if img(r).get("has_code"))
    abst = sum(1 for r in sel if img(r).get("declared")
               and not img(r).get("acc_native") and not img(r).get("has_code"))
    print(f"  {kind:<16}{len(sel):>7}{absent:>8}{undecl:>8}{nat:>8}{code:>8}{abst:>10}")

bridges = [r for r in rows if r["kind"] == "bridge"]
bad = [r for r in bridges if not img(r).get("acc_native")]
print(f"\n  BRIDGE rows with no ACC_NATIVE target in the image: {len(bad)} of {len(bridges)}")
inherited_bad = [r for r in bad if not r["kind_stated"]]
print(f"    ...of which inherited an ambient set_category: {len(inherited_bad)}")
print(f"    ...and were actually dispatched this run:      "
      f"{sum(1 for r in bad if r['invocations'] > 0)}")

if INHERITED_TSV:
    # The hierarchy pass, so `undecl` stops being a superset.
    resolved = {}
    try:
        with open(INHERITED_TSV, encoding="utf-8") as fh:
            for line in fh:
                f = line.rstrip("\n").split("\t")
                if len(f) >= 6:
                    resolved[(f[0], f[1], f[2])] = (f[3], f[4], f[5])
    except OSError as exc:
        sys.exit(f"cannot read --inherited {INHERITED_TSV}: {exc}")
    print("\n=== 2b. what the 'undecl' rows actually resolve to (hierarchy) ===")
    split = Counter()
    creditable = []
    for r in rows:
        if not (img(r).get("image_has_class") and not img(r).get("declared")):
            continue
        verdict, declarer, mods = resolved.get(
            (r["class"], r["name"], r["descriptor"]), ("NOT-MEASURED", "-", "-"))
        bucket = verdict
        if verdict == "INHERITED":
            bucket = "INHERITED " + mods.split(",")[0]
            if "native" in mods:
                creditable.append((r, declarer))
        split[bucket] += 1
    for bucket, n in sorted(split.items(), key=lambda kv: -kv[1]):
        print(f"  {bucket:<22}{n:>7}")
    print(f"\n  ACC_NATIVE on a SUPERTYPE -- section 2 counts these as "
          f"unadjudicated and they are not: {len(creditable)}")
    for r, declarer in creditable[:20]:
        print(f"    {r['class']}.{r['name']}{r['descriptor']}  ->  {declarer}")
    if len(creditable) > 20:
        print(f"    ... and {len(creditable) - 20} more")
else:
    print("\n=== 2b. hierarchy split of the 'undecl' rows: NOT RUN ===")
    print("  `undecl` above is a superset of 'dead'. Pass --inherited <tsv>")
    print("  (from scripts/jdk-only-inherited-decl.sh) to break it out.")

print("\n=== 3. natives shadowing concrete bytecode (image has_code) ===")
if INHERITED_TSV:
    # `has_code` is asked of the NAMED class. A native over a method the
    # class inherits concretely is just as much a shadow, and this column
    # cannot see one.
    extra = sum(1 for r in rows
                if resolved.get((r["class"], r["name"], r["descriptor"]),
                                ("", "", ""))[0] == "INHERITED"
                and "code" in resolved[(r["class"], r["name"],
                                        r["descriptor"])][2])
    print(f"  + {extra} more shadow bytecode they INHERIT, invisible to the"
          f" has_code column below")
    print(f"    (true shadow population = the total below + {extra})")
shadow = [r for r in rows if img(r).get("has_code")]
print(f"  total {len(shadow)}; dispatched this run {sum(1 for r in shadow if r['invocations'] > 0)}")
by_kind = Counter(r["kind"] for r in shadow)
print(f"  by kind {dict(by_kind)}")

print("\n=== 4. unadjudicated BRIDGE rows by registering file ===")
groups = defaultdict(lambda: [0, 0, 0])  # rows, inherited, invoked
for r in bad:
    site = (r.get("registered_by") or "?").rsplit(":", 1)[0]
    g = groups[site]
    g[0] += 1
    g[1] += 0 if r["kind_stated"] else 1
    g[2] += 1 if r["invocations"] > 0 else 0
for site, (n, inh, inv) in sorted(groups.items(), key=lambda kv: -kv[1][0]):
    print(f"  {n:>6} rows  {inh:>6} inherited  {inv:>5} invoked   {site}")

print("\n=== 5. rows invoked this run, by kind ===")
inv = Counter(r["kind"] for r in rows if r["invocations"] > 0)
print(f"  {dict(inv)}  (distinct slots dispatched: {sum(inv.values())})")

print("\n=== 6. SyntheticStub rows the image DOES declare as ACC_NATIVE ===")
print("  (a real native tagged as a fake -- the 2026-07-14 regression direction)")
mis = [r for r in rows if r["kind"] == "synthetic-stub" and img(r).get("acc_native")]
print(f"  total {len(mis)}")
for r in mis[:25]:
    print(f"    {r['class']}.{r['name']}{r['descriptor']}  inv={r['invocations']}  "
          f"{r.get('registered_by')}")
if len(mis) > 25:
    print(f"    ... and {len(mis) - 25} more")

# --- 7. the machine-readable block ----------------------------------------
# Section 2's `bridge` row again, in the shape the L6 ratchet freezes.  The
# five buckets are disjoint and sum to the Bridge total, which section 2's
# columns deliberately do NOT (its `code` column counts every has_code row of
# that kind, whether or not the method is also declared elsewhere in the
# table).  Read this one when you want an identity that adds up.
block = ratchet.adjudicate(doc)
print("\n=== 7. machine-readable adjudication block "
      "(scripts/jdk-only-bridge-ratchet.py) ===")
print(ratchet.render_block(block))
print("\n  gate it against the committed baseline with:")
print("    sh regression-suite/bridge-ratchet.sh")

if json_out:
    text = json.dumps(block, indent=2, sort_keys=True) + "\n"
    if json_out == "-":
        sys.stdout.write(text)
    else:
        with open(json_out, "w", encoding="utf-8") as fh:
            fh.write(text)
        print(f"\n  block written to {json_out}")
