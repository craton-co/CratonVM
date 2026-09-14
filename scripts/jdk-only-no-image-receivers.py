#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Does `native-api/src/no_image_receiver.rs` still match the images?

WHY THIS EXISTS
---------------

`NO_IMAGE_JDK_RECEIVERS` is a measurement frozen into Rust: the JDK-namespaced
receiver classes that **no supported JDK image declares**, so that
`NativeMethodRegistry::register` can tag their natives `SyntheticStub` instead
of letting them claim to be §1.5 `Bridge`s.

A frozen measurement drifts in two directions, and they are not symmetrical:

  * a name that GAINS a declaration must leave the table. Leaving it in demotes
    a real `ACC_NATIVE` bridge to a stub, `--jdk-only` drops it, and the
    failure surfaces far from here. That is the 2026-07-14
    `java.util.Properties` shape, and it is the reason this script exits
    non-zero rather than printing advice.
  * a name that LOSES its last declaration should join the table. Missing it
    costs nothing today — the row keeps an honest-but-wrong `Bridge` tag and
    keeps working — so it is reported as a suggestion, not a failure.

Usage:
    python3 scripts/jdk-only-no-image-receivers.py \\
        --images reg-linux25.json reg-linux21.json reg-windows25.json \\
                 reg-windows21.json reg-macos25.json reg-macos21.json \\
        --source native-api/src/no_image_receiver.rs

    python3 scripts/jdk-only-no-image-receivers.py --selftest

Exit codes:
    0  the table matches the images (suggestions may still be printed)
    1  a listed name IS declared by one of the images — the table is stale in
       the dangerous direction
    2  refused to adjudicate (census not image-adjudicated, unparseable source)
    3  a prerequisite is missing
"""
import argparse
import json
import re
import sys

JDK_NAMESPACES = ("java/", "javax/", "jdk/", "sun/", "com/sun/")
TABLE_RE = re.compile(
    r"pub const NO_IMAGE_JDK_RECEIVERS:\s*&\[&str\]\s*=\s*&\[(.*?)\];",
    re.DOTALL,
)
ENTRY_RE = re.compile(r'"([^"]+)"')


def parse_table(path):
    """The Rust table, as a set. Deliberately a regex and not a Rust parse.

    The table is a flat list of string literals with no `cfg`, no macro and no
    concatenation, and the unit test in the same file asserts it stays sorted
    and unique. A parser would be more machinery guarding a shape that is
    already pinned from the other side.
    """
    with open(path, encoding="utf-8") as fh:
        src = fh.read()
    m = TABLE_RE.search(src)
    if not m:
        sys.exit("REFUSING: no `pub const NO_IMAGE_JDK_RECEIVERS: &[&str] = &[…];`"
                 " in %s. If the table was renamed or restructured, update this"
                 " script rather than letting it report an empty table as a"
                 " clean result." % path)
    names = ENTRY_RE.findall(m.group(1))
    if not names:
        sys.exit("REFUSING: the table in %s parsed to zero entries. An empty"
                 " table would agree with any census." % path)
    return names


def load(path):
    with open(path, encoding="utf-8") as fh:
        doc = json.load(fh)
    if not doc.get("image_adjudication"):
        sys.exit("REFUSING: %s has image_adjudication false — re-run the VM"
                 " with --explain-jdk-only, or every class below reads as"
                 " absent for the wrong reason." % path)
    return doc["natives"]


def declared_classes(rows):
    """Class names this image has, from `image_has_class`."""
    return {
        r["class"]
        for r in rows
        if (r.get("image_declaring_method") or {}).get("image_has_class")
    }


def score(images, table):
    """(stale, suggested) — `images` is a list of (label, declared-class-set)."""
    listed = set(table)
    stale = []
    for name in table:
        where = [label for label, have in images if name in have]
        if where:
            stale.append((name, where))
    everywhere_absent = set()
    all_classes = set()
    for _, have in images:
        all_classes |= have
    return stale, everywhere_absent, all_classes, listed


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--images", nargs="*", default=[],
                    help="one census per (version, platform) image")
    ap.add_argument("--source", default="native-api/src/no_image_receiver.rs")
    ap.add_argument("--selftest", action="store_true",
                    help="exercise the scoring on synthetic input; no VM, no JDK")
    args = ap.parse_args(argv[1:])

    if args.selftest:
        return selftest()

    if len(args.images) < 2:
        sys.exit("REFUSING: give at least two censuses. One image cannot say"
                 " that a class is on no image.")
    joined = " ".join(args.images).lower()
    missing = [p for p in ("linux", "windows", "mac") if p not in joined]
    if missing:
        sys.exit("REFUSING: no census filename mentions %s. The table's whole"
                 " claim is 'no supported image declares this', and a sweep"
                 " missing a platform cannot make it — `sun/nio/ch/KQueuePort`"
                 " is the worked example." % " or ".join(missing))

    table = parse_table(args.source)
    images = []
    for path in args.images:
        label = path.rsplit("/", 1)[-1]
        images.append((label, declared_classes(load(path))))

    stale, _, all_classes, listed = score(images, table)

    print("source:  %s  (%d entries)" % (args.source, len(table)))
    print("images:  %s" % ", ".join(label for label, _ in images))

    # The suggestion direction: registered on a JDK-namespaced class that no
    # image has, and not in the table. Computed from the union of every row's
    # class, so a class nobody registers on is never suggested.
    registered = set()
    for path in args.images:
        for r in load(path):
            registered.add(r["class"])
    suggested = sorted(
        c for c in registered
        if c.startswith(JDK_NAMESPACES) and c not in all_classes and c not in listed
    )

    if stale:
        print("\nSTALE — listed here, but DECLARED by an image (%d):" % len(stale))
        for name, where in stale:
            print("  %-60s declared by %s" % (name, ", ".join(where)))
        print("\nEach of these is a real class on at least one supported image,"
              " so its\nnatives may be genuine §1.5 bridges. Remove the entry"
              " and re-take the\ncensus; leaving it demotes a bridge to a stub"
              " and `--jdk-only` drops it.")
    else:
        print("\nSTALE: none. Every listed name is absent from all %d images."
              % len(images))

    if suggested:
        print("\nSUGGESTED — registered, JDK-namespaced, on no image, not listed"
              " (%d):" % len(suggested))
        for name in suggested:
            print("  %s" % name)
        print("\nNot a failure: these keep a `Bridge` tag that §1.5 does not"
              " support, which\nis wrong on paper and inert in practice. Add"
              " them when you next touch the\ntable, after checking each is"
              " not a reviewed VM service.")
    else:
        print("SUGGESTED: none.")

    return 1 if stale else 0


def selftest():
    """The gate's own logic, on synthetic input.

    A guard nobody has watched fail is decoration. Both directions are
    injected: a listed name that an image declares must be reported STALE, and
    a listed name absent everywhere must not be.
    """
    table = ["java/lang/Compiler", "sun/nio/ch/KQueuePort"]
    images = [
        ("reg-linux25.json", {"java/lang/String"}),
        ("reg-windows25.json", {"java/lang/String"}),
        ("reg-macos25.json", {"java/lang/String", "sun/nio/ch/KQueuePort"}),
    ]
    stale, _, _, _ = score(images, table)
    got = {name for name, _ in stale}
    if got != {"sun/nio/ch/KQueuePort"}:
        print("SELFTEST FAILED: expected KQueuePort to be reported stale on the"
              " macOS arm, got %s" % sorted(got), file=sys.stderr)
        return 3
    stale, _, _, _ = score(images[:2], table)
    if stale:
        print("SELFTEST FAILED: nothing should be stale without the macOS arm,"
              " got %s" % sorted(n for n, _ in stale), file=sys.stderr)
        return 3
    print("selftest ok: the macOS-only class is caught with the macOS arm and"
          " missed without it — which is exactly why --images refuses a sweep"
          " that omits a platform.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
