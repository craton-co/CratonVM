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

And the ROW COUNT itself is a superset of "registrations that matter": the
census emits one row per registration, not per slot, so a triple registered
twice appears twice and only the last one can be dispatched.  Measured on
JDK 25 (2026-08-06): 1,237 of 11,876 rows own no slot, and 1,092 of them are in
the unadjudicated `Bridge` population below -- an 11 % overstatement of the
reclassification backlog.  Section 2 breaks it out from the `owns_slot`
column.
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

# Schema 4 resolves `image_declaring_method` up the hierarchy, so `undecl` is
# no longer a superset of "dead": a row that inherits a declaration says so on
# the row, and `nowhere` below is the genuinely-dead bucket. HIERARCHY is True
# when the census can answer that itself; when it is False every number here is
# the schema-3 reading and `--inherited` is the only way to split it.
HIERARCHY = any("inherited_from" in img(r) for r in rows)


def declared_anywhere(r):
    i = img(r)
    return i.get("declared") or i.get("inherited_from") is not None


def acc_native_anywhere(r):
    """Does the image declare this triple ACC_NATIVE anywhere that ADJUDICATES?

    Not simply "anywhere": `java.lang.Object` declares `hashCode`, `clone`,
    `getClass`, `notify`, `notifyAll` and `wait` ACC_NATIVE and everything
    inherits them, so a plain hierarchy answer would discharge any
    `X.hashCode()I` Bridge on any receiver. The census states the resolution
    because it is factually right; the ratchet and this script both decline to
    credit it. `ratchet._inherits_from_object` is the same rule, and the two
    must not drift.
    """
    i = img(r)
    if i.get("acc_native"):
        return True
    return bool(i.get("inherited_acc_native")) and not ratchet._inherits_from_object(i)


def has_code_anywhere(r):
    i = img(r)
    return i.get("has_code") or i.get("inherited_has_code")


print("\n=== 2. kind x what the IMAGE says (hierarchy-resolved: %s) ===" % HIERARCHY)
print(f"  {'kind':<16}{'rows':>7}{'absent':>8}{'nowhere':>9}{'native':>8}{'code':>8}"
      f"{'abstract':>10}{'i-native':>10}{'i-code':>8}{'i-abs':>7}")
for kind in ("intrinsic", "bridge", "synthetic-stub"):
    sel = [r for r in rows if r["kind"] == kind]
    absent = sum(1 for r in sel if not img(r).get("image_has_class"))
    nowhere = sum(1 for r in sel
                  if img(r).get("image_has_class") and not declared_anywhere(r))
    nat = sum(1 for r in sel if img(r).get("acc_native"))
    code = sum(1 for r in sel if img(r).get("has_code"))
    abst = sum(1 for r in sel if img(r).get("declared")
               and not img(r).get("acc_native") and not img(r).get("has_code"))
    inat = sum(1 for r in sel if img(r).get("inherited_acc_native"))
    icode = sum(1 for r in sel if img(r).get("inherited_has_code"))
    iabs = sum(1 for r in sel if img(r).get("inherited_abstract"))
    print(f"  {kind:<16}{len(sel):>7}{absent:>8}{nowhere:>9}{nat:>8}{code:>8}{abst:>10}"
          f"{inat:>10}{icode:>8}{iabs:>7}")
if not HIERARCHY:
    print("  WARNING: this census predates schema 4. `nowhere` above is really")
    print("  `class present, method not declared HERE`, which is roughly four")
    print("  times too large, and the three i-* columns are structurally zero.")

bridges = [r for r in rows if r["kind"] == "bridge"]
bad = [r for r in bridges if not acc_native_anywhere(r)]
print(f"\n  BRIDGE rows with no ACC_NATIVE target ANYWHERE in the hierarchy: "
      f"{len(bad)} of {len(bridges)}")
if HIERARCHY:
    credited = sum(1 for r in bridges if img(r).get("inherited_acc_native"))
    print(f"    (credited by the hierarchy pass; counted as unadjudicated "
          f"before schema 4: {credited})")
inherited_bad = [r for r in bad if not r["kind_stated"]]
print(f"    ...of which inherited an ambient set_category: {len(inherited_bad)}")
print(f"    ...and were actually dispatched this run:      "
      f"{sum(1 for r in bad if r['invocations'] > 0)}")

# A registration that no longer owns its slot cannot be dispatched, so its kind
# decides nothing -- but it is still a row here, and every reader of this table
# has been sizing the reclassification backlog off a number that includes them.
# This is the fourth way this census gets misread (the other three are in
# `census-asks-one-class-on-one-platform.md`): a *superseded* registration
# counted as a live one.
#
# `owns_slot` is a census column, not an inference. It was briefly derivable
# from row order -- within a triple the last row owns the slot -- and that is an
# ordering guarantee no external script should be resting on.
if any("owns_slot" in r for r in rows):
    superseded_bad = [r for r in bad if not r.get("owns_slot", True)]
    live_bad = len(bad) - len(superseded_bad)
    print(f"    ...superseded (own no slot, can never dispatch): {len(superseded_bad)}")
    print(f"    => LIVE unadjudicated BRIDGE surface:            {live_bad}")
    print(f"       (the {len(bad)} above is L6's ratchet population and is left"
          f" whole on purpose)")
    if superseded_bad:
        by_file = Counter(r["registered_by"].split(":")[0]
                          for r in superseded_bad if r.get("registered_by"))
        print("       superseded rows by registrar (top 8):")
        for where, n in by_file.most_common(8):
            print(f"         {n:>5}  {where}")
else:
    print("    ...superseded split: NOT AVAILABLE -- this census predates the")
    print("       `owns_slot` column, so an unknown share of the rows above own")
    print("       no slot and cannot be dispatched. Re-dump with a current build.")

if HIERARCHY:
    print("\n=== 2b. what the rows the class does NOT declare resolve to ===")
    print("  (from the census itself -- schema 4 walks the hierarchy inside")
    print("   ClassManager::adjudicate_natives_against_image, so neither")
    print("   jdk-only-inherited-decl.sh nor a HotSpot run is needed)")
    split = Counter()
    creditable = []
    for r in rows:
        i = img(r)
        if not i.get("image_has_class") or i.get("declared"):
            continue
        if i.get("inherited_acc_native"):
            if ratchet._inherits_from_object(i):
                split["INHERITED native (java.lang.Object - NOT credited)"] += 1
                continue
            split["INHERITED native"] += 1
            creditable.append((r, i.get("inherited_from")))
        elif i.get("inherited_has_code"):
            split["INHERITED code"] += 1
        elif i.get("inherited_abstract"):
            split["INHERITED abstract"] += 1
        else:
            split["NOWHERE"] += 1
    for bucket, n in sorted(split.items(), key=lambda kv: -kv[1]):
        print(f"  {bucket:<22}{n:>7}")
    print(f"\n  ACC_NATIVE on a SUPERTYPE -- section 2 used to count these as "
          f"unadjudicated and they are not: {len(creditable)}")
    for r, declarer in creditable[:20]:
        print(f"    {r['class']}.{r['name']}{r['descriptor']}  ->  {declarer}")
    if len(creditable) > 20:
        print(f"    ... and {len(creditable) - 20} more")
elif INHERITED_TSV:
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
    print("  This census predates schema 4 and no --inherited TSV was given, so")
    print("  `nowhere` above is a superset of 'dead'. Re-dump with a current")
    print("  build (preferred), or pass --inherited <tsv> from")
    print("  scripts/jdk-only-inherited-decl.sh.")

print("\n=== 3. natives shadowing concrete bytecode (image has_code) ===")
if HIERARCHY:
    extra = sum(1 for r in rows if img(r).get("inherited_has_code"))
    print(f"  + {extra} more shadow bytecode they INHERIT, counted separately")
    print("    because the has_code column is a statement about the NAMED class")
    print(f"    (true shadow population = the total below + {extra})")
elif INHERITED_TSV:
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

# ---------------------------------------------------------------------------
# Section 3b exists because every gate in this tree watches the wrong direction.
#
# L6's ratchet counts `Bridge` rows with no `ACC_NATIVE` target and refuses a
# rise. `jdk-only-kind-map.py` freezes each row's kind and refuses a change.
# Neither asks whether a `kind_stated` row's *claim* is true — and `kind_stated`
# is exactly the column readers treat as "somebody checked this against the
# image". Measured 2026-08-10 on JDK 25/linux: 87 of the 867 stated `Bridge`
# rows have no `ACC_NATIVE` target here, and reading them cost three separate
# checks to sort:
#
#   * 59 are `ACC_NATIVE` on ANOTHER supported image (`WinNTFileSystem`,
#     `WindowsSocketOptions`, `PlatformGraphicsInfo.hasDisplays0`) — correctly
#     stated, and a single-image census cannot say so;
#   * 4 inherit an `ACC_NATIVE` supertype method (`ComponentSampleModel.initIDs`
#     from `SampleModel`, three `FileDispatcherImpl.*` from
#     `UnixFileDispatcherImpl`) — also correctly stated, and `--inherited` is
#     what shows it;
#   * 24 registrations / 12 triples on `java/util/concurrent/ForkJoinTask`,
#     `RecursiveTask` and `RecursiveAction` are concrete bytecode on all six
#     supported images. Those are §1.4 shadows wearing a §1.5 claim.
#
# So the section prints the count unconditionally and the *unexplained* subset
# only when `--inherited` is available to discharge the second bucket. Without
# it, it says so rather than listing four rows it cannot judge.
print("\n=== 3b. rows that STATE Bridge with no ACC_NATIVE target here ===")
overstated = [r for r in rows
              if r["kind"] == "bridge" and r.get("kind_stated")
              and not acc_native_anywhere(r)]
print(f"  {len(overstated)} of {sum(1 for r in rows if r['kind'] == 'bridge' and r.get('kind_stated'))} stated Bridge rows")
if overstated:
    if HIERARCHY:
        # The inherited-ACC_NATIVE bucket is discharged by the census itself, so
        # every row left here is EITHER a platform-variant class this image
        # lacks (correct) OR a statement the image contradicts.
        print("  Rows that inherit an ACC_NATIVE supertype method are already")
        print("  discharged. Each row left is EITHER a platform-variant class this")
        print("  image lacks (correct) OR a statement the image contradicts; only")
        print("  a second image tells those apart:")
        print("    python3 scripts/jdk-only-platform-diff.py <this> <other> ...")
        shadowing = [r for r in overstated if has_code_anywhere(r)]
        print(f"  {len(shadowing)} of them target CONCRETE BYTECODE on this image --")
        print("  a contract-1.4 shadow wearing a 1.5 claim, which no image set can")
        print("  excuse. Those are statements to correct, not registrations to move.")
        by_class = Counter(r["class"] for r in shadowing)
        for cls, n in by_class.most_common(12):
            print(f"    {n:5d}  {cls}")
    elif INHERITED_TSV:
        unexplained = [
            r for r in overstated
            if "native" not in resolved.get(
                (r["class"], r["name"], r["descriptor"]), ("", "", ""))[2]
        ]
        print(f"  {len(unexplained)} of them do NOT inherit an ACC_NATIVE"
              f" supertype method either.")
        print("  Each is EITHER a platform-variant class this image lacks"
              " (correct) OR a\n  statement the image contradicts. Only a"
              " second image can tell them apart:\n"
              "    python3 scripts/jdk-only-platform-diff.py <this> <other> ...")
        by_class = Counter(r["class"] for r in unexplained)
        for cls, n in by_class.most_common(12):
            print(f"    {n:5d}  {cls}")
    else:
        print("  hierarchy split NOT RUN — pass --inherited <tsv> to discharge"
              "\n  the rows that inherit an ACC_NATIVE supertype method.")

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
