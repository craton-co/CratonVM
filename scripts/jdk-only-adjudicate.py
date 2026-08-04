#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Read a schema-3 native census and print the adjudication item 1 needs.

Usage:
    cratonvm --real-jdk --java-home <JDK> --explain-jdk-only \\
        --dump-native-registry census.json -cp probes JdkOnlyCensusLoadProbe
    python3 scripts/jdk-only-adjudicate.py census.json

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
  4. Which source file each unadjudicated group comes from, so the
     reclassification wave can be cut into subsystem-sized batches.
"""
import json
import sys
from collections import Counter, defaultdict

path = sys.argv[1]
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

print("\n=== 3. natives shadowing concrete bytecode (image has_code) ===")
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
