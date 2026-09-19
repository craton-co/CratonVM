#!/usr/bin/env python3
"""One table over every definition-of-done report, screened BY ROW.

The roadmap's §6 predicate is "no fabricated class instantiated, whatever its
package -- screened against the refused-class set the VM reports, not against a
prefix", so nothing here filters on a name. Every distinct
`compatibility-class-requested` row in every report is listed with its
requester, and the arm's own result line says whether the caller recovered.
"""
import collections
import json
import os
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else "/data/dod-out"
ARMS = sys.argv[2].split(",") if len(sys.argv) > 2 else [
    "sbsimple", "tcssl", "tcnetssl", "jdbc", "h2jdbc"]


def result_line(arm, mode):
    p = f"{OUT}/run-{arm}-{mode}.out"
    if not os.path.exists(p):
        return "(no log)"
    last = "(no DOD RESULT line)"
    lines = 0
    with open(p, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            lines += 1
            if line.startswith("DOD RESULT"):
                last = line.strip()
    return f"{last}  [lines={lines}]"


rows = []
fabs = collections.defaultdict(set)
for arm in ARMS:
    for mode in ("hotspot", "compat", "strict"):
        res = result_line(arm, mode)
        rep = f"{OUT}/rep-{arm}-{mode}.json"
        cc = fabn = shadows = syn = "-"
        partial = trunc = ""
        if mode != "hotspot" and os.path.exists(rep):
            d = json.load(open(rep, encoding="utf-8"))
            counts = d.get("counts", {})
            cc = counts.get("compatibility_classes", "?")
            syn = counts.get("synthetic_stub_invocations", "?")
            v = d.get("violations") or []
            f = [x for x in v if x.get("kind") == "compatibility-class-requested"]
            names = {x.get("class") for x in f}
            fabn = len(names)
            for x in f:
                fabs[(x.get("class"), x.get("requester"))].add(f"{arm}/{mode}")
            shadows = sum(1 for x in v
                          if x.get("kind") == "native-shadows-bytecode"
                          and x.get("outcome") == "native-won")
            if d.get("partial"):
                partial = " PARTIAL!"
            sink = d.get("observation_sink") or {}
            if sink.get("truncated") or sink.get("dropped"):
                trunc = " SINK-TRUNCATED!"
        elif mode != "hotspot":
            cc = fabn = shadows = syn = "NO-REPORT"
        rows.append((arm, mode, cc, syn, fabn, shadows, res + partial + trunc))

w = max(len(r[0]) for r in rows)
print(f"{'arm':<{w}}  {'mode':<8} {'compat_cls':>10} {'syn_stub':>8} "
      f"{'fab_req':>7} {'nativewon':>9}  result")
for r in rows:
    print(f"{r[0]:<{w}}  {r[1]:<8} {str(r[2]):>10} {str(r[3]):>8} "
          f"{str(r[4]):>7} {str(r[5]):>9}  {r[6]}")

print()
print("Fabrication requests, by row (no prefix filter):")
if not fabs:
    print("  (none in any report)")
for (cls, req), where in sorted(fabs.items()):
    print(f"  {cls}")
    print(f"      requester: {req}")
    print(f"      seen in  : {', '.join(sorted(where))}")
