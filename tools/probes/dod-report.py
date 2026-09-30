#!/usr/bin/env python3
"""Screen one --jdk-only-report against the roadmap's definition of done.

    "no fabricated class instantiated, whatever its package -- screened against
     the refused-class set the VM reports, not against a prefix."

So: no name filter of any kind is applied anywhere in this file. Every
`compatibility-class-requested` row the report carries is printed with its
requester and its reason.

Three things it prints that a reader would otherwise assume:

  * `partial: true` -- the report the System.exit path produces. It is MISSING
    the compatibility-class rows and the class buckets, so its silence about
    fabrication is not evidence. Flagged loudly.
  * `observation_sink.truncated` / `.dropped` -- the §7 shadow rows come out of
    a bounded sink (256 distinct rows by default). A truncated list looks
    exactly like a complete one.
  * the `native-shadows-bytecode` outcome split. `bytecode-won` is a native
    that lost the dispatch; counting it as a shadow inflates the worklist.
"""
import collections
import json
import sys


def main(path):
    with open(path, encoding="utf-8") as fh:
        d = json.load(fh)

    print(f"== {path}")
    print(f"   schema={d.get('schema_version')} mode={d.get('mode')} "
          f"jdk_feature={d.get('jdk_feature')} partial={d.get('partial', False)}")
    if d.get("partial"):
        print("   !! PARTIAL REPORT -- class buckets and compatibility-class rows are "
              "ABSENT, not zero. Do not read this as a clean run.")

    counts = d.get("counts", {})
    if counts:
        print("   counts: " + json.dumps(counts, sort_keys=True))
    else:
        print("   counts: (omitted -- partial report)")
    refusals = d.get("refusals")
    if refusals:
        print("   refusals: " + json.dumps(refusals, sort_keys=True))
    sink = d.get("observation_sink")
    if sink:
        print("   observation_sink: " + json.dumps(sink, sort_keys=True))
        flags = []
        if sink.get("truncated"):
            flags.append("truncated")
        if sink.get("saturated"):
            flags.append("saturated")
        if sink.get("dropped"):
            flags.append(f"dropped={sink['dropped']}")
        for sub in ("jit_fastpath", "jit_compile"):
            s = sink.get(sub) or {}
            if s.get("truncated") or s.get("dropped"):
                flags.append(f"{sub}:truncated={s.get('truncated')},dropped={s.get('dropped')}")
        if flags:
            print("   !! SINK NOT COMPLETE: " + ", ".join(flags)
                  + " -- violations[] is a FLOOR for the shadow rows.")

    v = d.get("violations") or []
    by = collections.Counter(x.get("kind", "?") for x in v)
    print(f"   violations: {len(v)} rows")
    for k, n in sorted(by.items()):
        print(f"     {k:32s} {n}")

    # ---- the DoD predicate itself -------------------------------------------
    cc = counts.get("compatibility_classes") if counts else None
    fab = [x for x in v if x.get("kind") == "compatibility-class-requested"]
    names = collections.Counter(x.get("class", "?") for x in fab)
    print(f"   -- DoD: compatibility_classes={cc}   "
          f"fabrication requests: {len(names)} distinct classes")
    for x in sorted(fab, key=lambda r: r.get("class", "")):
        print(f"      class    : {x.get('class')}")
        print(f"      requester: {x.get('requester')}")
        print(f"      loader   : {x.get('initiating_loader')}   reason: {x.get('reason')}")

    # ---- the shadow rows, split by who actually won -------------------------
    sh = [x for x in v if x.get("kind") == "native-shadows-bytecode"]
    if sh:
        out = collections.Counter(x.get("outcome", "(none)") for x in sh)
        print("   -- native-shadows-bytecode by outcome: " + json.dumps(dict(out), sort_keys=True))
        won = [x for x in sh if x.get("outcome") == "native-won"]
        trip = collections.Counter(
            (x.get("class", "?"), x.get("method", "?"), x.get("descriptor", "?")) for x in won)
        print(f"      native-won: {len(won)} rows, {len(trip)} distinct triples")
        fam = collections.Counter(t[0] for t in trip)
        for c, n in fam.most_common(25):
            print(f"        {n:4d}  {c}")

    miss = [x for x in v if x.get("kind") in ("missing-native", "missing-boot-class",
                                              "missing-implementation")]
    if miss:
        print(f"   -- missing-* rows: {len(miss)}")
        for x in miss[:40]:
            print(f"      {x.get('kind')}: {x.get('class')}.{x.get('method')}{x.get('descriptor') or ''}")

    syn = [x for x in v if x.get("kind") == "synthetic-native-invocation"]
    if syn:
        print(f"   !! synthetic-stub INVOCATIONS: {len(syn)}")
        for x in syn[:40]:
            print(f"      {x.get('class')}.{x.get('method')}{x.get('descriptor') or ''}"
                  f"  call_site={x.get('call_site')}")


if __name__ == "__main__":
    for p in sys.argv[1:]:
        main(p)
        print()
