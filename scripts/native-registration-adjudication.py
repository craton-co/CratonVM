#!/usr/bin/env python3
"""Turn a directory of per-vector registry dumps into one adjudication table.

    # 1. one strict-mode boot per scheduled vector, each with its own dump
    scripts/native-registration-census.sh <cratonvm> <outdir>

    # 2. the table, for any class prefix
    python3 scripts/native-registration-adjudication.py <outdir> java/io/

# What this answers that a single dump does not

An adjudication acts on a REGISTRATION, so it needs to know, per triple:

    inv    invocations UNIONED over every vector -- the traffic. A single boot
           answers this for one program; a lane deciding whether a row is dead
           needs the corpus.
    vecs   how many vectors invoked it at all. `inv=540, vecs=1` and
           `inv=540, vecs=36` are different situations.
    dup    how many OTHER files register the same triple. This is the `H11-2`
           section 4 hazard, counted rather than assumed: retiring a winner
           PROMOTES the loser, and 162 triples in this tree are registered more
           than once.
    code   whether the real JDK image declares the method WITH bytecode.
           `Y` = this row is a contract section 1.4 shadow; `-` = the image
           declares no such method, so the row is a stand-in for something
           absent rather than a shadow of something present; `N` = declared but
           `ACC_NATIVE` or `Code`-less, which a bridge is entitled to serve.
    kind   the registered `NativeKind`, which decides whether `--jdk-only`
           refuses it at the door.

Sorted so the cheap decisions come first: zero traffic, no duplicate
registrant, real bytecode present.

# The trap this exists to stop

A file's REGISTRATION COUNT is not its surface. `native-builtins/src/
deprecated_io_util.rs` registers 38 triples and OWNS 6; the other 32 lost their
slot to a later registrar and are inert. Adjudicating from a grep over
`r.register(` lines counts all 38. See `WORKER-4-2` section 3.

Written for `WORKER-4-2`, which used it on `java/io/`; nothing in it is
`java.io`-specific.
"""
import collections
import json
import os
import sys


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    dumpdir = sys.argv[1]
    prefix = sys.argv[2] if len(sys.argv) > 2 else ""

    inv = collections.Counter()
    vecs = collections.Counter()
    meta = {}
    files = collections.defaultdict(set)
    scanned = 0

    for name in sorted(os.listdir(dumpdir)):
        if not name.endswith(".json"):
            continue
        try:
            rows = json.load(open(os.path.join(dumpdir, name)))["natives"]
        except Exception as exc:                       # a truncated dump
            print("SKIP %s (%s)" % (name, exc), file=sys.stderr)
            continue
        scanned += 1
        for r in rows:
            if not r["class"].startswith(prefix):
                continue
            key = (r["class"], r["name"], r["descriptor"])
            files[key].add(r.get("registered_by", "?").split(":")[0])
            if not r.get("owns_slot"):
                continue
            real = r.get("real_declaring_method") or {}
            meta[key] = (
                r.get("registered_by", "?"),
                r.get("kind", "?"),
                bool(real.get("declared")),
                bool(real.get("has_code")),
                bool(real.get("acc_native")),
            )
            n = r.get("invocations") or 0
            if n:
                inv[key] += n
                vecs[key] += 1

    shadows = sum(
        1 for k in meta if meta[k][2] and meta[k][3] and not meta[k][4]
    )
    print("vectors unioned: %d      owned triples under %r: %d      of those, "
          "section 1.4 shadows: %d      unreached: %d"
          % (scanned, prefix or "<all>", len(meta), shadows,
             sum(1 for k in meta if not inv[k])))
    print()
    print("%-34s %-24s %-44s %7s %5s %4s %5s %-15s %s"
          % ("class", "method", "descriptor", "inv", "vecs", "dup", "code",
             "kind", "registered_by"))
    for key in sorted(meta, key=lambda k: (inv[k], -len(files[k]), k)):
        by, kind, declared, has_code, acc_native = meta[key]
        if not declared:
            code = "-"
        elif has_code and not acc_native:
            code = "Y"
        else:
            code = "N"
        print("%-34s %-24s %-44s %7d %5d %4d %5s %-15s %s"
              % (key[0], key[1], key[2][:44], inv[key], vecs[key],
                 len(files[key]) - 1, code, kind, by))
    return 0


if __name__ == "__main__":
    sys.exit(main())
