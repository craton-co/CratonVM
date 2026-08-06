#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Which registrations are dead on EVERY supported JDK image, and dispatched by
nothing?

WHY THIS EXISTS
---------------

Three separate readings of the census have called some bucket "the deletion
list", and all three were wrong, each for a different reason:

  * `ABSENT` on one image is not dead — the class may be the correct one for
    the other platform (`WinNTFileSystem`, `WindowsSocketOptions`).
  * `ABSENT` on both platforms of one JDK is not dead either — a registration
    dead on 25 may be the live one on 21.
  * A name in a JDK package is not necessarily a JDK class. This VM mints
    `java/util/HashMap$KeyItr`, `java/util/TreeSet$Itr`,
    `java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl` and
    `java/util/function/Function$Identity`, none of which any JDK declares —
    and they are dispatched thousands of times.

So this takes a census per (version, platform) image, intersects them, and then
subtracts everything any workload actually dispatched. What survives is dead on
every image the project supports AND unreached: the honest candidates.

    for arm in linux21 linux25 windows21 windows25; do
        cratonvm --real-jdk --java-home <image-$arm> --explain-jdk-only \\
            --dump-native-registry reg-$arm.json -cp probes <Probe>
    done
    sh scripts/jdk-only-inherited-decl.sh reg-linux25.json inh.tsv
    python3 scripts/jdk-only-dead-sweep.py --images reg-*.json \\
        --inherited inh.tsv --dispatched reg-load.json reg-breadth.json reg-h2.json

`--inherited` matters: `declared: false` on one class is not "the method is
nowhere", and 1,919 of 2,522 such rows resolve to a supertype. Without it this
tool over-reports by roughly four times, which is exactly the mistake it exists
to stop repeating.

Result on JDK 21.0.12 + 25.0.4, linux + windows, three workloads, 2026-08-05:
**796 registrations** — 243 whose class no image has, 553 whose method is
nowhere in its hierarchy on any of them. Nine candidates were removed by the
dispatch filter, and every one of the nine was a VM-minted class wearing a JDK
name.
"""
import argparse
import json
import sys
from collections import Counter

JDK_NAMESPACES = ("java/", "javax/", "jdk/", "sun/", "com/sun/")


def load(path):
    with open(path, encoding="utf-8") as fh:
        doc = json.load(fh)
    if not doc.get("image_adjudication"):
        sys.exit("REFUSING: %s has image_adjudication false — re-run the VM with "
                 "--explain-jdk-only, or every verdict below is null for the "
                 "wrong reason." % path)
    return doc["natives"]


def verdict(row):
    img = row.get("image_declaring_method") or {}
    if not img.get("image_has_class"):
        return "ABSENT"
    if not img.get("declared"):
        return "UNDECL"
    if img.get("acc_native"):
        return "NATIVE"
    if img.get("has_code"):
        return "CODE"
    return "ABSTRACT"


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--images", nargs="+", required=True,
                    help="one census per (version, platform) image")
    ap.add_argument("--inherited", required=True,
                    help="TSV from scripts/jdk-only-inherited-decl.sh")
    ap.add_argument("--dispatched", nargs="*", default=[],
                    help="censuses whose invocation counts disqualify a row")
    ap.add_argument("--out", help="write the surviving list here as TSV")
    args = ap.parse_args(argv[1:])

    if len(args.images) < 2:
        sys.exit("REFUSING: one image cannot answer 'dead everywhere'. Give at "
                 "least two, and cover both platforms if the project ships on "
                 "both.")

    arms = [(p, load(p)) for p in args.images]
    n = len(arms[0][1])
    for path, rows in arms:
        if len(rows) != n:
            sys.exit("REFUSING: %s has %d rows, the first has %d. Same VM "
                     "binary and same workload for every arm — only "
                     "--java-home may differ." % (path, len(rows), n))

    not_found = set()
    for line in open(args.inherited, encoding="utf-8"):
        f = line.rstrip("\n").split("\t")
        if len(f) >= 4 and f[3] == "NOT-FOUND":
            not_found.add((f[0], f[1], f[2]))

    dispatched = Counter()
    for path in args.dispatched:
        for r in load(path):
            if r["invocations"]:
                dispatched[(r["class"], r["name"], r["descriptor"])] += r["invocations"]

    absent, nowhere, live = [], [], []
    for i in range(n):
        row = arms[0][1][i]
        key = (row["class"], row["name"], row["descriptor"])
        vs = [verdict(rows[i]) for _, rows in arms]
        if not row["class"].startswith(JDK_NAMESPACES):
            continue                      # not a name a JDK image owes us
        if all(v == "ABSENT" for v in vs):
            bucket = absent
        elif all(v in ("ABSENT", "UNDECL") for v in vs) and key in not_found:
            bucket = nowhere
        else:
            continue
        (live if dispatched.get(key) else bucket).append((row, dispatched.get(key, 0)))

    print("images:     %s" % ", ".join(p.rsplit("/", 1)[-1] for p, _ in arms))
    print("rows:       %d" % n)
    print("workloads:  %d census(es), %d slots dispatched between them"
          % (len(args.dispatched), sum(1 for v in dispatched.values() if v)))
    print("\nclass ABSENT from every image:                 %d" % len(absent))
    print("method NOWHERE in its hierarchy on any image:  %d" % len(nowhere))
    print("DEAD EVERYWHERE AND UNREACHED:                 %d" % (len(absent) + len(nowhere)))

    print("\ndisqualified by the dispatch filter: %d" % len(live))
    for row, inv in sorted(live, key=lambda t: -t[1]):
        print("  inv=%-7d %s.%s%s" % (inv, row["class"], row["name"], row["descriptor"]))
    if live:
        print("  Every one of these is alive despite the images. A JDK-shaped "
              "name\n  can still be a class this VM mints — check before "
              "believing a census\n  that says a `java.util` class does not "
              "exist.")

    survivors = absent + nowhere
    print("\nby kind: %s" % dict(Counter(r["kind"] for r, _ in survivors)))
    by_file = Counter((r.get("registered_by") or "?").rsplit(":", 1)[0] for r, _ in survivors)
    print("by registering file:")
    for f, c in by_file.most_common(15):
        print("  %5d  %s" % (c, f))

    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write("# class\tmethod\tdescriptor\tkind\tregistered_by\tbucket\n")
            for rows_, tag in ((absent, "class-absent"), (nowhere, "method-nowhere")):
                for r, _ in sorted(rows_, key=lambda t: (t[0]["class"], t[0]["name"],
                                                         t[0]["descriptor"])):
                    fh.write("%s\t%s\t%s\t%s\t%s\t%s\n"
                             % (r["class"], r["name"], r["descriptor"], r["kind"],
                                r.get("registered_by"), tag))
        print("\nwritten: %s" % args.out)
    print("\nThis is a candidate list, not a delete-me list: it covers the "
          "images swept\nand the workloads run, and nothing else.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
