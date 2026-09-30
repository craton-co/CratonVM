#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Which loaded classes does a native on an ABSTRACT method intercept?

WHY THIS EXISTS
---------------

A native registered on an abstract method does not shadow one implementation —
it intercepts **every** implementor, including classes the application defines.
That is the standing hazard on `register_interface_natives`, and the 308
inherited-abstract rows in
`jdk-only-census-one-class-one-platform-FIXED-20260810.md` are the
same hazard in a second place. The native census counts *invocations* but has
never said what they were dispatched against, so the blast radius was
unmeasured.

This joins three artefacts from ONE run:

  --registry      the native census      (--dump-native-registry --explain-jdk-only)
  --classes       the class-origin census (--dump-class-origins), whose rows
                  carry `supertypes` — the direct superclass and interfaces of
                  every class the run loaded
  --inherited     optional: the TSV from scripts/jdk-only-inherited-decl.sh, so
                  natives whose target is abstract on a SUPERTYPE are included
                  rather than filed under "method not declared"

and reports, per abstract-target native, the loaded subtypes it stands in front
of, split by origin. A subtype from `application-class-path` or `user-defined`
is a class the *application* wrote and this VM silently re-implements.

WHAT IT MEASURES, AND WHAT IT DOES NOT
--------------------------------------

The interception **surface**: every loaded class that inherits the intercepted
method. Not a per-invocation receiver log — the VM's dispatch sites do not all
hold the receiver, and threading one through them would put work on the hottest
path in the interpreter for a diagnostic. The surface is the better number for
deciding a kind anyway: it is what the registration *can* capture, and it does
not depend on whether this particular workload happened to call it.

Usage:
    python3 scripts/jdk-only-interception.py --registry census.json \\
        --classes classes.json [--inherited undecl-out.tsv] [--only-user]
"""
import argparse
import json
import sys
from collections import defaultdict

USER_ORIGINS = ("application-class-path", "user-defined")


def load_json(path, what):
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except OSError as exc:
        sys.exit("cannot read the %s census (%s): %s" % (what, path, exc))


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--registry", required=True, help="--dump-native-registry JSON")
    ap.add_argument("--classes", required=True, help="--dump-class-origins JSON")
    ap.add_argument("--inherited", help="TSV from jdk-only-inherited-decl.sh")
    ap.add_argument("--only-user", action="store_true",
                    help="report only natives that intercept an application class")
    args = ap.parse_args(argv[1:])

    registry = load_json(args.registry, "native registry")
    if not registry.get("image_adjudication"):
        sys.exit("REFUSING: the registry census has image_adjudication false. "
                 "Re-run the VM with --explain-jdk-only; without it nothing "
                 "below can tell an abstract target from any other.")
    classes = load_json(args.classes, "class-origin")

    rows = classes.get("classes", [])
    if rows and "supertypes" not in rows[0]:
        sys.exit("REFUSING: this class-origin census has no `supertypes` column. "
                 "It was written by a VM older than the column; re-take it, or "
                 "the interception sets below would all be empty and read as a "
                 "clean result.")

    # child -> direct parents, and the origin of every loaded class.
    parents = {}
    origin = {}
    for row in rows:
        parents.setdefault(row["name"], set()).update(row.get("supertypes") or ())
        origin[row["name"]] = row["origin"]

    # Transitive: for each loaded class, everything above it.
    def ancestors(name, _memo={}):
        if name in _memo:
            return _memo[name]
        _memo[name] = set()          # cycle guard; a broken graph must not hang
        out = set()
        for p in parents.get(name, ()):  # noqa: B007
            out.add(p)
            out |= ancestors(p)
        _memo[name] = out
        return out

    subtypes = defaultdict(set)
    for name in parents:
        for a in ancestors(name):
            subtypes[a].add(name)

    # Natives whose target is abstract: declared abstract on the named class,
    # or (with --inherited) abstract on a supertype.
    inherited_abstract = set()
    if args.inherited:
        for line in open(args.inherited, encoding="utf-8"):
            f = line.rstrip("\n").split("\t")
            if len(f) >= 6 and f[3] == "INHERITED" and "abstract" in f[5]:
                inherited_abstract.add((f[0], f[1], f[2]))

    reported = 0
    user_hits = 0
    print("%-6s %-8s %s" % ("subs", "user", "native (declaring class . method)"))
    for r in registry["natives"]:
        img = r.get("image_declaring_method") or {}
        key = (r["class"], r["name"], r["descriptor"])
        is_abstract = (img.get("declared") and not img.get("acc_native")
                       and not img.get("has_code")) or key in inherited_abstract
        if not is_abstract:
            continue
        subs = subtypes.get(r["class"], set())
        users = sorted(s for s in subs if origin.get(s) in USER_ORIGINS)
        if args.only_user and not users:
            continue
        reported += 1
        if users:
            user_hits += 1
        print("%-6d %-8d %s.%s%s  kind=%s inv=%s"
              % (len(subs), len(users), r["class"], r["name"], r["descriptor"],
                 r["kind"], r["invocations"]))
        for u in users[:8]:
            print("           user: %s" % u)
        if len(users) > 8:
            print("           ... and %d more" % (len(users) - 8))

    print("\nabstract-target natives reported: %d" % reported)
    print("of which intercept at least one APPLICATION class: %d" % user_hits)
    print("\nA subtype list is only as wide as what this workload loaded. An "
          "empty one means 'nothing loaded under it here', never 'nothing can'.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
