#!/usr/bin/env python3
"""The union census over a corpus of `--jdk-only-report` files.

One report answers for one program. A corpus answers for a workload, and the
two differ in the direction that matters: the per-vector sinks are BOUNDED, so a
union is the only way to get a shadow population that is not silently every
vector's first 256 rows.

Prints, in this order, because that is the order a reader has to trust them in:

  1. how many reports there are, and how many vectors produced NONE -- an absent
     report is not a clean vector, it is an unmeasured one;
  2. whether any report is `partial` or hit a sink cap -- if so, every count
     below it is a FLOOR and the page says so;
  3. the definition-of-done predicate, per vector and unioned;
  4. every `compatibility-class-requested` row, by class, with requesters and
     the number of vectors that asked;
  5. the `native-shadows-bytecode` worklist split by `outcome`, unioned over the
     corpus -- `bytecode-won` is a native that ALREADY LOST and has nothing to
     retire, so only `native-won` is a worklist.
"""
import collections
import json
import os
import sys

REP = sys.argv[1] if len(sys.argv) > 1 else "/data/corpus/strict/rep"
EXPECTED = int(sys.argv[2]) if len(sys.argv) > 2 else 0

files = sorted(f for f in os.listdir(REP) if f.endswith(".json"))
print(f"== reports: {len(files)}" + (f" of {EXPECTED} vectors" if EXPECTED else ""))
if EXPECTED and len(files) < EXPECTED:
    print(f"   !! {EXPECTED - len(files)} vector(s) produced NO report. Those are "
          f"UNMEASURED, not clean: a missing file is what a System.exit, a "
          f"timeout or a flag-order mistake looks like.")

partial, truncated, saturated = [], [], []
cc_nonzero, syn_nonzero = [], []
fab = collections.defaultdict(lambda: {"requesters": set(), "vectors": set()})
won = collections.Counter()
lost = collections.Counter()
kinds = collections.Counter()
app_classes = 0

for f in files:
    v = f[:-5]
    try:
        d = json.load(open(os.path.join(REP, f), encoding="utf-8"))
    except Exception as e:                      # a truncated write is a result
        print(f"   !! {v}: unreadable ({e.__class__.__name__})")
        continue
    if d.get("partial"):
        partial.append(v)
    sink = d.get("observation_sink") or {}
    if sink.get("truncated") or sink.get("dropped"):
        truncated.append(v)
    if sink.get("saturated"):
        saturated.append(v)
    counts = d.get("counts") or {}
    if counts.get("compatibility_classes"):
        cc_nonzero.append((v, counts["compatibility_classes"]))
    if counts.get("synthetic_stub_invocations"):
        syn_nonzero.append((v, counts["synthetic_stub_invocations"]))
    app_classes += counts.get("application_classes", 0)
    for x in d.get("violations") or []:
        k = x.get("kind", "?")
        kinds[k] += 1
        if k == "compatibility-class-requested":
            e = fab[x.get("class", "?")]
            e["requesters"].add(x.get("requester") or "?")
            e["vectors"].add(v)
        elif k == "native-shadows-bytecode":
            t = (x.get("class"), x.get("method"), x.get("descriptor"))
            (won if x.get("outcome") == "native-won" else lost)[t] += 1

print()
print("== completeness")
print(f"   partial reports : {len(partial)}" + (f"  {partial[:5]}" if partial else ""))
print(f"   sink truncated  : {len(truncated)}" + (f"  {truncated[:5]}" if truncated else ""))
print(f"   sink saturated  : {len(saturated)}" + (f"  {saturated[:5]}" if saturated else ""))
if truncated or saturated or partial:
    print("   !! every count below is a FLOOR, not a total.")
else:
    print("   every report is complete: the counts below are totals.")

print()
print("== the definition-of-done predicate, over the corpus")
print(f"   vectors with compatibility_classes > 0      : {len(cc_nonzero)}")
for v, n in cc_nonzero[:20]:
    print(f"      {v}  {n}")
print(f"   vectors with synthetic_stub_invocations > 0 : {len(syn_nonzero)}")
for v, n in syn_nonzero[:20]:
    print(f"      {v}  {n}")

print()
print(f"== fabrication requests: {len(fab)} distinct classes (no prefix filter)")
for cls, e in sorted(fab.items(), key=lambda kv: (-len(kv[1]["vectors"]), kv[0])):
    print(f"   {len(e['vectors']):4d} vectors  {cls}")
    for r in sorted(e["requesters"]):
        print(f"                 requester {r}")

print()
print("== violation kinds, unioned")
for k, n in sorted(kinds.items()):
    print(f"   {k:32s} {n}")

print()
print(f"== native-shadows-bytecode: {len(won)} distinct native-won triples, "
      f"{len(lost)} bytecode-won")
print("   a bytecode-won row is a native that already LOST the dispatch: it is "
      "not a worklist entry.")
fam = collections.Counter()
for (c, _m, _d), n in won.items():
    fam[c] += 1
for c, n in fam.most_common(30):
    print(f"   {n:4d}  {c}")
