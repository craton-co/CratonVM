#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Which registrations are dead on EVERY supported JDK image, and dispatched by
nothing?

WHY THIS EXISTS
---------------

Four separate readings of the census have called some bucket "the deletion
list", and all four were wrong, each for a different reason:

  * `ABSENT` on one image is not dead — the class may be the correct one for
    the other platform (`WinNTFileSystem`, `WindowsSocketOptions`).
  * `ABSENT` on both platforms of one JDK is not dead either — a registration
    dead on 25 may be the live one on 21.
  * `ABSENT` on linux AND windows is *still* not dead: `sun/nio/ch/KQueuePort`
    and `sun/nio/fs/PollingWatchService` are on both macOS images, and a
    four-image sweep put 13 of their rows on the deletion list.
  * A name in a JDK package is not necessarily a JDK class. This VM mints
    `java/util/HashMap$KeyItr`, `java/util/TreeSet$Itr`,
    `java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl` and
    `java/util/function/Function$Identity`, none of which any JDK declares —
    and they are dispatched thousands of times.

So this takes a census per (version, platform) image, intersects them, and then
subtracts everything any workload actually dispatched.

    for arm in linux21 linux25 windows21 windows25 macos21 macos25; do
        cratonvm --real-jdk --java-home <image-$arm> --explain-jdk-only \\
            --dump-native-registry reg-$arm.json -cp probes <Probe>
    done
    sh scripts/jdk-only-inherited-decl.sh reg-linux25.json inh.tsv
    python3 scripts/jdk-only-dead-sweep.py --images reg-*.json \\
        --inherited inh.tsv --dispatched reg-load.json reg-breadth.json reg-reach.json

`--inherited` matters: `declared: false` on one class is not "the method is
nowhere", and 1,919 of 2,522 such rows resolve to a supertype. Without it this
tool over-reports by roughly four times, which is exactly the mistake it exists
to stop repeating.

WHAT THIS TOOL WILL NOT DO ANY MORE, AND WHY
--------------------------------------------

**`class ABSENT from every image` is no longer a deletion bucket.** It is
reported, and it is written to the *gated* section, next to the synthetic
stubs. Three measurements, 2026-08-10:

  * `apps/probes/DeadSweepReachProbe.java` — one workload written against the
    committed list rather than against JDK surface — dispatched **30 of its
    791 rows**, every one a VM-minted class wearing a JDK name. Eight classes
    ended up split down the middle: `AtomicIntegerFieldUpdater$RustJvmImpl` had
    4 of its 12 registrations dispatched and the other 8 on the deletion list,
    separated by nothing but which methods a probe happened to call.
  * Adding the two macOS images moved 13 more rows out.
  * Every one of the fourteen legacy names left over — `java/lang/Compiler`,
    `java/lang/UNIXProcess`, `sun/misc/Cleaner`, `sun/reflect/Reflection`,
    `java/net/PlainSocketImpl`, `sun/nio/ch/WindowsFileDispatcherImpl` and the
    rest — **resolves under `--synthetic-jdk`**, where the VM mints a
    `compatibility-stub` stand-in on demand and these registrations are its
    only implementation. That is the fifth image, and this tool cannot census
    it: `image_declaring_method` has no bytes to parse.

The dispatch filter can only ever subtract what a workload reached, so it
cannot decide a class that exists on demand. The disposition for the whole
bucket is therefore a *kind*, not a deletion:
`native-api/src/no_image_receiver.rs` tags those receivers
`NativeKind::SyntheticStub` at registration, which gates them out of
`--jdk-only` and leaves `Compatible` and `--synthetic-jdk` untouched.

`method NOWHERE in its hierarchy on any image` is different and stays a
candidate list: there the class IS a real JDK class, so the receiver's
existence is not in question, only the method's.

Result on JDK 21.0.12 + 25.0.4, linux + windows + macos, three workloads,
2026-08-10: **199 gated** (class absent everywhere) and **549 candidates**
(method nowhere in the hierarchy).
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
    ap.add_argument("--minted", help="newline-separated class names the SOURCE "
                                     "passes to a fabrication funnel; every "
                                     "row on one of them is gated")
    ap.add_argument("--pinned", help="native-builtins/tests/registry_contracts.rs; "
                                     "every triple it pins is a deliberate "
                                     "completion of the synthetic surface and is "
                                     "gated, whatever an image census says")
    ap.add_argument("--out", help="write the surviving list here as TSV")
    ap.add_argument("--gated-out",
                    help="write the gated (never-delete) rows here as TSV")
    ap.add_argument("--allow-partial-platforms", action="store_true",
                    help="score anyway when the image set does not name all "
                         "three platforms — for a one-off diff, never for a "
                         "list anyone will act on")
    args = ap.parse_args(argv[1:])

    if len(args.images) < 2:
        sys.exit("REFUSING: one image cannot answer 'dead everywhere'. Give at "
                 "least two, and cover both platforms if the project ships on "
                 "both.")

    # Platform coverage, from the census filenames. Crude on purpose: the
    # census does not record which OS its image was for, and inferring it from
    # the class set would be a guess this tool then presents as a fact.
    #
    # It is checked because the omission is not hypothetical. The list this
    # tool wrote on 2026-08-05 was built from linux+windows only, and put
    # `sun/nio/ch/KQueuePort` (4 rows) and `sun/nio/fs/PollingWatchService`
    # (9 rows) in the "class is in no image" bucket. Both are in both macOS
    # images. A missing platform reads exactly like a dead class.
    joined = " ".join(args.images).lower()
    missing = [p for p in ("linux", "windows", "mac") if p not in joined]
    if missing and not args.allow_partial_platforms:
        sys.exit("REFUSING: no census filename mentions %s, so this image set "
                 "cannot say whether a class is missing from every platform or "
                 "only from the ones you swept. `sun/nio/ch/KQueuePort` is the "
                 "worked example. Add the arm, or pass "
                 "--allow-partial-platforms and do not write a list."
                 % " or ".join(missing))
    if missing:
        print("WARNING: platforms not covered by this image set: %s. Verdicts "
              "below are 'absent from the images swept', not 'absent "
              "everywhere'.\n" % ", ".join(missing))

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

    # ------------------------------------------------------------------
    # THE FIFTH-IMAGE RULE IS A PROPERTY OF THE CLASS, NOT OF THE TRIPLE.
    #
    # Added 2026-08-10, and it is a correction rather than a tightening: the
    # rule below used to be applied per (class, method, descriptor), through
    # the `synthetic-stub` kind tag and the dispatch filter. Both are
    # triple-granular, and "this VM mints the class" is not.
    #
    # What that cost, measured against the committed baseline: 242 of its 791
    # rows are on a class this tree fabricates, and among them were
    # `java/util/HashMap$KeyItr.remove()V` — the write-through `remove()` that
    # `RChmKeySetView` exercises and that
    # `ensure-synthetic-class-cannot-enforce-only-record` argues must not be
    # traded away — plus the whole
    # `Atomic{Integer,Long,Reference}FieldUpdater$RustJvmImpl` surface, which is
    # every method those updaters have. The dispatch filter removed `hasNext`
    # and `next` because the three workloads called them 1,209 times; it left
    # `remove` because they never did. A method the sample did not reach is not
    # a dead method, and for a class no image contains the census can say
    # nothing else — which is exactly what the rule already said.
    #
    # So: a class ABSENT from every image, of which ANY triple is dispatched or
    # tagged `synthetic-stub`, is a class this VM mints. Every row of it is
    # gated.
    class_rows = {}
    for i in range(n):
        class_rows.setdefault(arms[0][1][i]["class"], []).append(i)
    minted_classes = set()
    for cls, idxs in class_rows.items():
        if not all(
            all(verdict(rows[i]) == "ABSENT" for _, rows in arms) for i in idxs
        ):
            continue                      # some image has this class
        for i in idxs:
            row = arms[0][1][i]
            key = (row["class"], row["name"], row["descriptor"])
            if dispatched.get(key) or row["kind"] == "synthetic-stub":
                minted_classes.add(cls)
                break

    # A second, census-independent answer to the same question, for the classes
    # no workload happened to touch at all: the tree names them. `--minted`
    # takes a newline-separated list of class names the source passes to a
    # fabrication funnel (`try_alloc_synthetic`, `try_alloc_concurrent_synthetic`,
    # `try_ensure_synthetic_class`, `ensure_vm_internal_class`, …). A class this
    # VM allocates and no image declares is minted whether or not this run
    # reached it.
    source_minted = set()
    if args.minted:
        with open(args.minted, encoding="utf-8") as fh:
            source_minted = {ln.strip() for ln in fh if ln.strip()}

    # And a third answer, for the rows where the class IS in every image and the
    # METHOD is in none: a method the JDK never declared can still be a
    # deliberate completion of the synthetic surface, which is what
    # `StampedLock.isLocked()` is. A contract test pinning the triple is the
    # statement of intent; the census cannot see it, and deleting such a row
    # breaks the test that exists to say so.
    #
    # `--pinned` takes a FILE OR A DIRECTORY, and pointing it at one file is how
    # 179 registrations were deleted on 2026-08-10 with 24 tests pinning them.
    # `registry_contracts.rs` is not the only place the tree states this intent:
    # `.find(class, method, descriptor)` inside an `assert!` is the idiom, and it
    # appears in `preconditions.rs`, `shared_secrets_bridge.rs`,
    # `file_channel.rs`, `deprecated_verify.rs`, `http_client.rs`,
    # `inet_address.rs`, `jca/provider_chain.rs` and `native-io/src/lib.rs` among
    # others. Given a directory this walks every `.rs` under it.
    #
    # Two literal shapes are read, because the tree uses both:
    #   ("class", "method", "descriptor")           -- tuple tables
    #   .find(class_or_binding, "method", "desc")   -- direct assertions
    # `const`/`let` string bindings are resolved within the same file, which is
    # what `let fci = "sun/nio/ch/FileChannelImpl";` needs.
    #
    # A pin is a CLAIM, not proof. `file_channel.rs` pinned an
    # `…ZZZLjava/lang/Object;` spelling of `FileChannelImpl.open` that no JDK
    # declares, so the pin and the registration agreed with each other and with
    # nothing else, and JDK 21 went uncovered for as long as both stood. Pins
    # keep a row off the deletion list; they do not make it right.
    pinned = set()
    if args.pinned:
        import os as _os
        import re as _re
        if _os.path.isdir(args.pinned):
            paths = [_os.path.join(root, f)
                     for root, _dirs, files in _os.walk(args.pinned)
                     for f in files if f.endswith(".rs")]
        else:
            paths = [args.pinned]
        # The trailing `,?` is not cosmetic: rustfmt breaks a three-element
        # tuple across four lines and leaves a comma before the `)`, which is
        # how `shared_secrets_bridge.rs`'s 15-owner table is written. Without it
        # this parser read 38 pins where the tree states 1,775.
        tuple_re = _re.compile(r'\(\s*(\w+|"[^"]+"),\s*"([^"]+)",\s*"([^"]+)",?\s*\)')
        find_re = _re.compile(r'\.find\(\s*(\w+|"[^"]+"),\s*"([^"]+)",\s*"([^"]+)",?\s*\)')
        # The other shape a pin takes: a table of (method, descriptor) PAIRS
        # looped over a class named once, as a literal, in the `.find` itself —
        # `preconditions.rs` and `native-io/src/lib.rs` both do this. The class
        # is recoverable, the pairs are, and nothing else in these files looks
        # like a `("name", "(descriptor)")` tuple, so the pairs are attributed
        # to every literal class the file probes. Over-pinning is the safe
        # direction: a pin only keeps a row OFF the deletion list.
        loopfind_re = _re.compile(r'\.find\(\s*"([^"]+/[^"]+)",\s*\w+,\s*\w+\s*\)')
        pair_re = _re.compile(r'\(\s*"([A-Za-z_$<][\w$<>]*)",\s*"(\([^"]*)"\s*,?\s*\)')
        bind_re = _re.compile(r'(?:const|let)\s+(\w+)(?:\s*:\s*&\s*\'?\w*\s*str)?\s*=\s*"([^"]+)"')
        for path in paths:
            try:
                src = open(path, encoding="utf-8").read()
            except OSError:
                continue
            binds = dict(bind_re.findall(src))
            for rx in (tuple_re, find_re):
                for mo in rx.finditer(src):
                    c, m, d = mo.groups()
                    cls = binds.get(c, c.strip('"'))
                    # Require a class-shaped name and a descriptor-shaped
                    # descriptor. A bare identifier the file never bound is a
                    # local whose value is unknowable here, and guessing would
                    # pin an arbitrary row.
                    if "/" not in cls or not d.startswith("("):
                        continue
                    # `.find(...).is_none()` is the OPPOSITE of a pin: it
                    # asserts the triple must NOT be registered. Reading one as
                    # a pin would protect exactly the rows somebody went to the
                    # trouble of forbidding — `file_channel.rs` forbids the
                    # `Object`-tailed `FileChannelImpl.open`, and the first
                    # version of this parser pinned it.
                    if "is_none" in src[mo.end():mo.end() + 60]:
                        continue
                    pinned.add((cls, m, d))
            for cls in set(loopfind_re.findall(src)):
                for m, d in pair_re.findall(src):
                    pinned.add((cls, m, d))
        print("pinned:     %d triples from %d file(s) under %s"
              % (len(pinned), len(paths), args.pinned))

    absent, nowhere, live, gated = [], [], [], []
    for i in range(n):
        row = arms[0][1][i]
        key = (row["class"], row["name"], row["descriptor"])
        vs = [verdict(rows[i]) for _, rows in arms]
        if not row["class"].startswith(JDK_NAMESPACES):
            continue                      # not a name a JDK image owes us
        class_absent = all(v == "ABSENT" for v in vs)
        if class_absent:
            bucket = absent
        elif all(v in ("ABSENT", "UNDECL") for v in vs) and key in not_found:
            bucket = nowhere
        else:
            continue
        if dispatched.get(key):
            live.append((row, dispatched[key]))
        elif (
            row["kind"] == "synthetic-stub"
            or row["class"] in minted_classes
            or row["class"] in source_minted
            or key in pinned
        ):
            # THE RULE. A synthetic stub is CratonVM's OWN implementation, and
            # no census of JDK images can adjudicate it: the census scores it
            # ABSENT precisely because no JDK owes us a class this VM mints
            # (`Comparator$Native`, `Function$Identity`, `HashMap$KeyItr`), or a
            # method the JDK never declared (`StampedLock.isLocked`). Where the
            # kind tag is present it is already gated — `NativeKind::SyntheticStub`
            # is the one kind `--jdk-only` rejects, so strict mode drops it and
            # the real bytecode wins — and in real-JDK mode it is inert rather
            # than wrong, because nothing can reach a class that only exists
            # when the VM minted it. Where the tag is absent (the ambient
            # `Bridge` default) the class-level test above is what catches it.
            #
            # Deleting one would remove the implementation the SYNTHETIC JDK
            # depends on, which is the one image this sweep never censuses —
            # or, worse, a capability the real one has: `HashMap$KeyItr.remove`
            # writes through to the map.
            # Gate, never delete.
            gated.append((row, 0,
                          "kind=synthetic-stub" if row["kind"] == "synthetic-stub"
                          else "pinned by a contract test" if key in pinned
                          else "class is minted by this VM"))
        elif class_absent:
            # THE SAME RULE, ARRIVED AT FROM THE OTHER SIDE — and this arm is
            # the one that was missing. The clause above gates a row because
            # somebody had already tagged it `synthetic-stub`; but the tag is
            # what a reclassification wave is *deciding*, so gating on it makes
            # the instrument agree with whatever the tree currently says
            # instead of adjudicating it.
            #
            # The image-side fact is the same in both cases: no supported image
            # declares the class. Then either this VM mints the receiver — and
            # the registration is that stand-in's implementation — or nothing
            # can ever produce one and the row decides nothing. Deletion is
            # wrong in the first case and pointless in the second, so neither
            # branch justifies it.
            #
            # Measured 2026-08-10, three ways: `DeadSweepReachProbe` dispatched
            # 30 rows this bucket had called dead; the macOS arms took out 13
            # more; and all fourteen surviving legacy names resolve under
            # `--synthetic-jdk`, where the VM mints a `compatibility-stub` for
            # each on demand. See the module docstring.
            gated.append((row, 0, "class in no supported image"))
        else:
            bucket.append((row, 0))

    print("images:     %s" % ", ".join(p.rsplit("/", 1)[-1] for p, _ in arms))
    print("rows:       %d" % n)
    print("workloads:  %d census(es), %d slots dispatched between them"
          % (len(args.dispatched), sum(1 for v in dispatched.values() if v)))
    # `absent` is now always empty — every class-absent row is gated above.
    # The addend is kept in the arithmetic so a future edit that reopens the
    # bucket cannot do it silently.
    print("\nclass ABSENT from every image (all GATED, see below):  %d"
          % sum(1 for _, _, why in gated if why == "class in no supported image"))
    print("method NOWHERE in its hierarchy on any image:          %d" % len(nowhere))
    print("DELETION CANDIDATES (method-nowhere, unreached):       %d"
          % (len(absent) + len(nowhere)))

    print("\ndisqualified by the dispatch filter: %d" % len(live))
    for row, inv in sorted(live, key=lambda t: -t[1]):
        print("  inv=%-7d %s.%s%s" % (inv, row["class"], row["name"], row["descriptor"]))
    if live:
        print("  Every one of these is alive despite the images. A JDK-shaped "
              "name\n  can still be a class this VM mints — check before "
              "believing a census\n  that says a `java.util` class does not "
              "exist.\n  This filter is a LOWER BOUND: it subtracts what these "
              "workloads reached,\n  which is why the class-absent bucket is "
              "gated by rule rather than by it.")

    gated_by_reason = Counter(why for _, _, why in gated)
    print("\ngated, and NOT deletion candidates: %d  %s"
          % (len(gated), dict(gated_by_reason)))
    for row, _, why in sorted(gated, key=lambda t: (t[2], t[0]["class"], t[0]["name"],
                                                    t[0]["descriptor"])):
        print("  [%s] %s.%s%s" % (why, row["class"], row["name"], row["descriptor"]))
    if gated:
        print("  These are CratonVM's own implementations. Where the row is "
              "tagged\n  `SyntheticStub`, `--jdk-only` already drops it and the "
              "real bytecode wins;\n  where the class is one this VM mints, no "
              "JDK-image census can adjudicate\n  it at all — the census scores "
              "it ABSENT because no JDK owes us the class.\n  A method the "
              "sampled workloads did not reach is NOT a dead method on such a\n"
              "  class. They are omitted from the written list on purpose.")
        if minted_classes:
            print("  classes gated wholesale (image-absent + minted here): %s"
                  % ", ".join(sorted(minted_classes)))
        print("  The runtime gate for the class-absent rows lives in\n"
              "  `native-api/src/no_image_receiver.rs`, which tags them "
              "`SyntheticStub` at\n  registration — so the `--minted` and "
              "`--pinned` lists above and that table\n  are two readings of one "
              "rule, not two rules.")

    survivors = absent + nowhere
    print("\nby kind: %s" % dict(Counter(r["kind"] for r, _ in survivors)))
    by_file = Counter((r.get("registered_by") or "?").rsplit(":", 1)[0] for r, _ in survivors)
    print("by registering file:")
    for f, c in by_file.most_common(15):
        print("  %5d  %s" % (c, f))

    if args.out:
        with open(args.out, "w", encoding="utf-8", newline="\n") as fh:
            fh.write("# class\tmethod\tdescriptor\tkind\tregistered_by\tbucket\n")
            for rows_, tag in ((absent, "class-absent"), (nowhere, "method-nowhere")):
                for r, _ in sorted(rows_, key=lambda t: (t[0]["class"], t[0]["name"],
                                                         t[0]["descriptor"])):
                    fh.write("%s\t%s\t%s\t%s\t%s\t%s\n"
                             % (r["class"], r["name"], r["descriptor"], r["kind"],
                                r.get("registered_by"), tag))
        print("\nwritten: %s" % args.out)
    if args.gated_out:
        with open(args.gated_out, "w", encoding="utf-8", newline="\n") as fh:
            fh.write("# class\tmethod\tdescriptor\tkind\tregistered_by\tgated_because\n")
            for r, _, why in sorted(gated, key=lambda t: (t[0]["class"], t[0]["name"],
                                                          t[0]["descriptor"])):
                fh.write("%s\t%s\t%s\t%s\t%s\t%s\n"
                         % (r["class"], r["name"], r["descriptor"], r["kind"],
                            r.get("registered_by"), why))
        print("written: %s" % args.gated_out)
    print("\nThis is a candidate list, not a delete-me list: it covers the "
          "images swept\nand the workloads run, and nothing else. Classes no "
          "supported image declares\nare excluded by rule — see the gated "
          "section above.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
