#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""The METHOD-granular multi-image sweep. `H25-1` N1.

WHY THIS EXISTS
---------------

`H25-1` measured **342 strict-mode registrations naming a method no JDK 25
image declares anywhere on the receiver's hierarchy** — and then corrected
itself in its own §1.6:

    java/lang/StringUTF16.isBigEndian()Z

is one of the 342, and `lang_string.rs:12422` carries a 56-line comment whose
heading is *"On JDK 25 this registration never fires, and that is not a
defect"* — it is kept for **JDK 17/21 images, which DO declare it**.

So **342 is a ONE-IMAGE UPPER BOUND, not a work list**, and until this sweep
runs no row in that population may be deleted by anyone.  That is the sentence
this script exists to remove.  Its output is the thing W3 and W4 are blocked
on: which rows are dead on *every* supported image (retirable) and which are
deliberate cross-version registrations (must not be touched).

The class-granular sibling is `scripts/jdk-only-no-image-receivers.py`, and the
rule it states is the one this extends, with *class* replaced by *method*:

    if no image on any supported (version, platform) pair declares the
    receiver METHOD anywhere on the receiver's hierarchy, there is no
    ACC_NATIVE method for the registration to bind to and there never can be

THE FIVE VERDICTS
-----------------

    no-image-class   no image declares the receiver CLASS. Already the
                     sibling's question; reported, not re-adjudicated here.
    dead-everywhere  at least one image HAS the class, and NO image declares
                     the method on the class or on any supertype/interface.
                     *** the retirable population ***
    cross-version    some image declares it, some image that HAS the class does
                     not. *** the isBigEndian shape — DO NOT DELETE ***
    field-shaped     no image declares it as a METHOD, but the NAME is on the
                     hierarchy as a FIELD. *** DO NOT DELETE ***
    live             every image that has the class declares the method.

`field-shaped` exists because this sweep was WRONG without it. It indexes the
method table, so a registration naming a field read as `dead-everywhere` and
went into the committed work list — **42 rows nominated for a retirement that
must not happen**. It was caught by comparing against WORKER 3's independent
`jdk-only-no-image-methods.py`, which shells out to `javap` and therefore sees
fields for free; that one reported 38 rows of the same shape. Two
implementations of one measurement, and the disagreement was the finding.

THERE ARE TWO SWEEPS IN THIS TREE, ON PURPOSE
---------------------------------------------

`scripts/jdk-only-no-image-methods.py` (WORKER 3) asks `javap -p -s --system`
per class. This one builds one in-process index per image and answers every
query from it, which is what makes the coverage refusals, the canary and a
committed TSV cheap enough to run over all 10,378 registrations. They agree on
the load-bearing categories — `WORKER-5-NOTE-1` §2.5 has the reconciliation —
and neither should be deleted without re-running the other.

`dead-everywhere` is split further, because the two halves are different bug
reports (`H25-1` §2.2):

    truly-gone   no image declares that method NAME on the hierarchy at all
    near-miss    some image declares the NAME but no overload with this
                 DESCRIPTOR — an interception somebody intended to install,
                 which has never once fired, and which nothing reports

Usage:
    python3 scripts/jdk-only-image-method-index.py --image ... --out idx/X.json.gz
    python3 scripts/jdk-only-image-method-sweep.py \\
        --census reg-jdkonly.json --indexes idx/*.json.gz --out sweep.json --tsv s.tsv

    python3 scripts/jdk-only-image-method-sweep.py --selftest

Exit codes:
    0  the sweep ran (findings are in the report; this is not a pass/fail gate)
    1  the CANARY misfired — a registration the tree documents as a deliberate
       cross-version keep did not classify as cross-version. The sweep is
       wrong; do not use its output.
    2  refused to adjudicate (census not image-adjudicated, image set too
       narrow, unreadable input)
    3  selftest failed
"""
import argparse
import glob
import gzip
import json
import sys

PLATFORMS = ("linux", "windows", "macos")

# The standing witness of §1.6. It is in the 342 on JDK 25 and DECLARED on
# 17/21, so a sweep that does not call it `cross-version` has a broken
# hierarchy walk, a missing old image, or a broken index — and every other row
# it prints is then untrustworthy. Checked on every real run, not only in the
# selftest: a gate nobody has watched fail is decoration.
CANARY = ("java/lang/StringUTF16", "isBigEndian", "()Z")


def load_index(path):
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt", encoding="utf-8") as fh:
        doc = json.load(fh)
    for key in ("label", "classes"):
        if key not in doc:
            sys.exit("REFUSING: %s has no `%s` — it is not an image index from"
                     " jdk-only-image-method-index.py." % (path, key))
    if len(doc["classes"]) < 1000:
        sys.exit("REFUSING: %s carries only %d classes. A thin index reports"
                 " every method as absent and would turn the whole census into"
                 " a work list." % (path, len(doc["classes"])))
    return doc


def load_census(path):
    with open(path, encoding="utf-8") as fh:
        doc = json.load(fh)
    if not doc.get("image_adjudication"):
        sys.exit("REFUSING: %s has image_adjudication false — re-run the VM with"
                 " --explain-jdk-only." % path)
    if doc.get("mode") not in (None, "jdk-only"):
        sys.exit("REFUSING: %s was taken in mode %r. This sweep is STRICT-MODE"
                 " ONLY: a --synthetic-jdk carrier CAN declare a method the real"
                 " image does not, so a compatible-mode census would nominate"
                 " rows that are load-bearing there." % (path, doc.get("mode")))
    return doc["natives"]


def declares(classes, cls, name, desc):
    """(exact, name_only, field) — does the hierarchy rooted at `cls` declare it?

    `exact` is name+descriptor found on the class, a superclass, or any
    (transitive) interface.  `name_only` is the same walk asking about the NAME
    alone, which is what separates `H25-1`'s 286 truly-gone from its 56
    near-misses.  Returns (None, None) when the image does not have the class
    at all — a distinct answer from "has it, does not declare it", and
    collapsing the two is how a one-platform sweep calls a macOS class dead.
    """
    if cls not in classes:
        return (None, None, None)
    key = name + desc
    seen = set()
    stack = [cls]
    exact = False
    name_only = False
    field = False
    while stack:
        c = stack.pop()
        if c in seen:
            continue
        seen.add(c)
        entry = classes.get(c)
        if entry is None:
            # A supertype outside this image (a third-party or platform class).
            # Not an error: the walk simply cannot see past it, and saying so
            # by continuing is honest — `exact` stays False and the row lands
            # in `cross-version` or `dead-everywhere` for a reason the report
            # names.
            continue
        if key in entry["m"]:
            exact = True
        if not name_only:
            for k in entry["m"]:
                if k.startswith(name) and k[len(name):len(name) + 1] == "(":
                    name_only = True
                    break
        # A registration whose name is a FIELD here is NOT a dead method.
        # WORKER 3's javap-based sweep found 38 such rows and this index, built
        # from the method table alone, called every one of them dead. An index
        # that cannot see fields must not be allowed to nominate them.
        if not field and name in entry.get("f", ()):
            field = True
        if exact and name_only and field:
            break
        if entry.get("s"):
            stack.append(entry["s"])
        stack.extend(entry.get("i") or [])
    return (exact, name_only, field)


def classify(row, images):
    """One registration against every image. Returns a verdict dict."""
    cls, name, desc = row["class"], row["name"], row["descriptor"]
    have, declared, name_seen, field_seen = [], [], [], []
    for label, classes in images:
        exact, name_only, field = declares(classes, cls, name, desc)
        if exact is None:
            continue
        have.append(label)
        if exact:
            declared.append(label)
        if name_only:
            name_seen.append(label)
        if field:
            field_seen.append(label)
    if not have:
        verdict = "no-image-class"
    elif not declared and field_seen:
        # The name IS on the hierarchy — as a FIELD. Not a dead method, and not
        # this sweep's business to nominate.
        verdict = "field-shaped"
    elif not declared:
        verdict = "dead-everywhere"
    elif len(declared) == len(have):
        verdict = "live"
    else:
        verdict = "cross-version"
    out = {
        "class": cls, "name": name, "descriptor": desc,
        "verdict": verdict,
        "owns_slot": row.get("owns_slot"),
        "registered_by": (row.get("registered_by") or "").replace("\\", "/"),
        "invocations": row.get("invocations"),
        "images_with_class": have,
        "images_declaring": declared,
    }
    if verdict == "dead-everywhere":
        out["shape"] = "near-miss" if name_seen else "truly-gone"
        out["images_declaring_the_name"] = name_seen
    if verdict == "cross-version":
        out["images_not_declaring"] = [x for x in have if x not in declared]
    if verdict == "field-shaped":
        out["images_declaring_the_field"] = field_seen
    return out


def coverage_refusals(images_meta):
    """Every reason to refuse this image set, as a list of sentences."""
    bad = []
    if len(images_meta) < 2:
        bad.append("only %d index(es) given. One image cannot say that a method"
                   " is on no image — that is exactly the one-image measurement"
                   " H25-1 retracted." % len(images_meta))
    plats = {m.get("platform") for m in images_meta}
    missing_p = [p for p in PLATFORMS if p not in plats]
    if missing_p:
        bad.append("no index is for %s. `sun/nio/ch/KQueuePort` is the worked"
                   " example of what a sweep missing a platform calls dead."
                   % " or ".join(missing_p))
    rels = sorted(r for r in {m.get("release") for m in images_meta} if r)
    if len(rels) < 2:
        bad.append("only release(s) %s. The whole point of this sweep is that"
                   " StringUTF16.isBigEndian is dead on 25 and declared on"
                   " 17/21; a single-release sweep cannot see that."
                   % (rels or "none"))
    elif min(rels) > 17:
        bad.append("the oldest release swept is %d. lang_string.rs:12422 names"
                   " JDK 17 explicitly as an image these registrations are kept"
                   " for, so a sweep starting at %d can still call a deliberate"
                   " row dead." % (min(rels), min(rels)))
    return bad


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--census", help="a --dump-native-registry --explain-jdk-only JSON")
    ap.add_argument("--indexes", nargs="*", default=[],
                    help="image indexes from jdk-only-image-method-index.py")
    ap.add_argument("--out", help="write the full per-row verdict JSON here")
    ap.add_argument("--tsv",
                    help="write the two ACTIONABLE verdicts (dead-everywhere and"
                         " cross-version) here as a sorted TSV — small enough to"
                         " commit, which is what makes it quotable next week")
    ap.add_argument("--allow-narrow", action="store_true",
                    help="run anyway on an image set that fails the coverage"
                         " check, and stamp the report `narrow: true`. For"
                         " development only — a narrow sweep may NOT be quoted"
                         " as authority to delete anything.")
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args(argv[1:])

    if args.selftest:
        return selftest()
    if not (args.census and args.indexes):
        sys.exit("REFUSING: --census and --indexes are both required.")

    paths = []
    for pattern in args.indexes:
        hits = sorted(glob.glob(pattern)) or [pattern]
        paths.extend(hits)

    docs = [load_index(p) for p in paths]
    bad = coverage_refusals(docs)
    if bad and not args.allow_narrow:
        sys.exit("REFUSING to adjudicate this image set:\n" +
                 "\n".join("  * " + s for s in bad) +
                 "\n\nPass --allow-narrow only to develop the tool; its output"
                 " is then not authority to delete anything.")

    images = [(d["label"], d["classes"]) for d in docs]
    rows = load_census(args.census)
    verdicts = [classify(r, images) for r in rows]

    # THE CANARY. Runs before anything is reported, on every real sweep.
    canary_rows = [v for v in verdicts
                   if (v["class"], v["name"], v["descriptor"]) == CANARY]
    canary = {"triple": "%s.%s%s" % CANARY, "present_in_census": bool(canary_rows)}
    if canary_rows:
        canary["verdict"] = canary_rows[0]["verdict"]
        canary["images_declaring"] = canary_rows[0]["images_declaring"]

    counts = {}
    for v in verdicts:
        counts[v["verdict"]] = counts.get(v["verdict"], 0) + 1
    shapes = {}
    for v in verdicts:
        if v["verdict"] == "dead-everywhere":
            shapes[v["shape"]] = shapes.get(v["shape"], 0) + 1

    print("census:  %s  (%d registrations)" % (args.census, len(rows)))
    print("images:  %s" % ", ".join("%s [%s]" % (d["label"], d.get("java_version") or "?")
                                    for d in docs))
    if bad:
        print("\n*** NARROW SWEEP — coverage check waived with --allow-narrow ***")
        for s in bad:
            print("  * %s" % s)
    print("\nverdicts:")
    for k in ("live", "cross-version", "field-shaped", "dead-everywhere",
              "no-image-class"):
        print("  %-16s %5d" % (k, counts.get(k, 0)))
    if shapes:
        print("  dead-everywhere splits: %s"
              % ", ".join("%s=%d" % kv for kv in sorted(shapes.items())))

    print("\ncanary %s: %s" % (canary["triple"],
                               canary.get("verdict", "NOT IN CENSUS")))

    cross = [v for v in verdicts if v["verdict"] == "cross-version"]
    if cross:
        print("\nCROSS-VERSION — declared by SOME image, absent from another."
              " DO NOT DELETE (%d):" % len(cross))
        for v in sorted(cross, key=lambda v: (v["class"], v["name"]))[:200]:
            print("  %-58s %-30s declared by %s"
                  % (v["class"] + "." + v["name"], v["descriptor"],
                     ",".join(v["images_declaring"])))
        if len(cross) > 200:
            print("  … %d more (full list in --out)" % (len(cross) - 200))

    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            json.dump({"census": args.census,
                       "images": [{k: d[k] for k in
                                   ("label", "release", "platform",
                                    "java_version", "class_count")} for d in docs],
                       "narrow": bool(bad),
                       "counts": counts,
                       "dead_shapes": shapes,
                       "canary": canary,
                       "rows": verdicts}, fh, indent=1, sort_keys=True)
        print("\nwrote %s" % args.out)

    if args.tsv:
        act = sorted((v for v in verdicts
                      if v["verdict"] in ("dead-everywhere", "cross-version",
                                          "field-shaped")),
                     key=lambda v: (v["verdict"], v["class"], v["name"], v["descriptor"]))
        with open(args.tsv, "w", encoding="utf-8", newline="\n") as fh:
            fh.write("# jdk-only method-granular multi-image sweep — H25-1 N1\n")
            fh.write("# regenerate: scripts/jdk-only-image-method-index.py per image,"
                     " then scripts/jdk-only-image-method-sweep.py --tsv\n")
            fh.write("# census: %s (%d registrations, strict mode)\n" % (args.census, len(rows)))
            fh.write("# images: %s\n" % " ".join("%s=%s" % (d["label"], d.get("java_version") or "?")
                                                 for d in docs))
            fh.write("# canary %s: %s\n" % (canary["triple"], canary.get("verdict", "ABSENT")))
            fh.write("# verdicts: %s\n" % " ".join("%s=%d" % kv for kv in sorted(counts.items())))
            fh.write("#\n")
            fh.write("# cross-version rows MUST NOT be deleted: an image this host does not\n")
            fh.write("# run declares the method.\n")
            fh.write("# field-shaped rows MUST NOT be deleted either: the name IS on the\n")
            fh.write("# hierarchy, as a FIELD. An index built from the method table alone calls\n")
            fh.write("# them dead and is wrong; WORKER 3's independent javap sweep found the\n")
            fh.write("# same shape (38 rows there, 42 here).\n")
            fh.write("# dead-everywhere rows are the retirable population, and retiring them is\n")
            fh.write("# predicted to move the shadow census by ZERO — they are never\n")
            fh.write("# dispatched (H25-1 §3).\n")
            fh.write("verdict\tshape\tclass\tname\tdescriptor\towns_slot\tregistered_by\tdeclared_by\n")
            for v in act:
                fh.write("%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n"
                         % (v["verdict"], v.get("shape", "-"), v["class"], v["name"],
                            v["descriptor"], "yes" if v["owns_slot"] else "no",
                            v["registered_by"],
                            ",".join(v["images_declaring"]) or "-"))
        print("wrote %s (%d actionable rows)" % (args.tsv, len(act)))

    if canary_rows and canary["verdict"] != "cross-version":
        print("\nCANARY MISFIRED: %s classified `%s`, expected `cross-version`."
              "\nlang_string.rs:12422 documents it as a deliberate keep for JDK"
              " 17/21. Either an old image is missing from --indexes, the"
              "\nhierarchy walk is broken, or an index is thin. Do NOT use this"
              " sweep's output." % (canary["triple"], canary["verdict"]),
              file=sys.stderr)
        return 1
    if not canary_rows:
        print("\nNOTE: the canary triple is not in this census, so the sweep's"
              " own\ncross-version detection went unexercised on real data.",
              file=sys.stderr)
    return 0


def selftest():
    """Every verdict, both `dead-everywhere` shapes, and every refusal.

    Built from synthetic indexes so it needs no JDK and no VM. `H25-1`'s two
    real rows are modelled by name: `isBigEndian` (declared on 21, gone on 25)
    must come out `cross-version`, and `Thread.destroy()V` (gone everywhere)
    must come out `dead-everywhere`.
    """
    old = {
        "java/lang/Object": {"s": None, "i": [], "m": {"toString()Ljava/lang/String;": 1}},
        "java/lang/StringUTF16": {"s": "java/lang/Object", "i": [],
                                  "m": {"isBigEndian()Z": 9}},
        "java/lang/Thread": {"s": "java/lang/Object", "i": [], "m": {"start()V": 1}},
        "java/lang/StringBuilder": {"s": "java/lang/Object", "i": [],
                                    "m": {"repeat(Ljava/lang/CharSequence;I)V": 1}},
        "java/util/HashMap": {"s": "java/lang/Object", "i": [], "m": {},
                              "f": {"table": 0}},
    }
    new = {k: dict(v) for k, v in old.items()}
    new["java/lang/StringUTF16"] = {"s": "java/lang/Object", "i": [], "m": {}}
    images = [("jdk21-linux", old), ("jdk25-linux", new)]

    def row(c, n, d):
        return {"class": c, "name": n, "descriptor": d, "owns_slot": True,
                "registered_by": "x.rs:1", "invocations": 0}

    cases = [
        (("java/lang/StringUTF16", "isBigEndian", "()Z"), "cross-version", None),
        (("java/lang/Thread", "destroy", "()V"), "dead-everywhere", "truly-gone"),
        (("java/lang/StringBuilder", "repeat", "(Ljava/lang/String;I)V"),
         "dead-everywhere", "near-miss"),
        (("java/lang/Compiler", "enable", "()V"), "no-image-class", None),
        (("java/lang/Thread", "start", "()V"), "live", None),
        # Inheritance: HashMap declares nothing, but Object does. A walk that
        # stopped at the class would call this dead and nominate it.
        (("java/util/HashMap", "toString", "()Ljava/lang/String;"), "live", None),
        # THE NAME IS A FIELD, not a method. An index built from the method
        # table alone calls this `dead-everywhere` and nominates it for
        # retirement; WORKER 3's javap sweep found 38 rows of this shape.
        (("java/util/HashMap", "table", "()V"), "field-shaped", None),
    ]
    for triple, want, want_shape in cases:
        got = classify(row(*triple), images)
        if got["verdict"] != want:
            print("SELFTEST FAILED: %s.%s%s -> %s, expected %s"
                  % (triple + (got["verdict"], want)), file=sys.stderr)
            return 3
        if want_shape and got.get("shape") != want_shape:
            print("SELFTEST FAILED: %s.%s%s shape -> %s, expected %s"
                  % (triple + (got.get("shape"), want_shape)), file=sys.stderr)
            return 3

    # A class present on only ONE image must not be judged by the images that
    # do not have it: `have` is the denominator, not the image list.
    macos_only = [("jdk25-linux", new),
                  ("jdk25-macos", dict(new, **{"sun/nio/ch/KQueuePort":
                                               {"s": None, "i": [], "m": {"close()V": 1}}}))]
    got = classify(row("sun/nio/ch/KQueuePort", "close", "()V"), macos_only)
    if got["verdict"] != "live":
        print("SELFTEST FAILED: a macOS-only class declaring the method must be"
              " `live`, not %s — the linux image not having the class is not"
              " evidence against it." % got["verdict"], file=sys.stderr)
        return 3

    # Every refusal path.
    meta = lambda rel, plat: {"release": rel, "platform": plat}
    checks = [
        ("one index", [meta(25, "linux")], "only 1 index"),
        ("missing platform", [meta(21, "linux"), meta(25, "linux")], "no index is for"),
        ("one release", [meta(25, "linux"), meta(25, "windows"), meta(25, "macos")],
         "only release"),
        ("too new", [meta(21, "linux"), meta(21, "windows"), meta(21, "macos"),
                     meta(25, "linux"), meta(25, "windows"), meta(25, "macos")],
         "oldest release swept"),
    ]
    for label, metas, needle in checks:
        msgs = coverage_refusals(metas)
        if not any(needle in m for m in msgs):
            print("SELFTEST FAILED: the %r image set was not refused with %r;"
                  " got %r" % (label, needle, msgs), file=sys.stderr)
            return 3
    full = [meta(r, p) for r in (17, 21, 25) for p in PLATFORMS]
    if coverage_refusals(full):
        print("SELFTEST FAILED: the full 3x3 sweep must NOT be refused; got %r"
              % coverage_refusals(full), file=sys.stderr)
        return 3

    print("selftest ok: all five verdicts (including field-shaped), both"
          " dead-everywhere shapes, inherited-from-Object, the platform-only"
          " class, and all four coverage refusals plus the 3x3 acceptance.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
