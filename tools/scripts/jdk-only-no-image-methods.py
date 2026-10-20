#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Which registered (class, method, descriptor) triples does NO supported JDK image declare?

WHY THIS EXISTS
---------------

`scripts/jdk-only-no-image-receivers.py` answers the same question at CLASS
granularity: a receiver class no supported image declares can carry no
`ACC_NATIVE` method, so its registrations are `SyntheticStub`, not §1.5
`Bridge`s. `H25-1` measured that the identical argument holds one level down --
342 registrations in a strict-mode dump name a METHOD no JDK 25 image declares
anywhere on the receiver's hierarchy -- and then refused to act on it, because a
one-image measurement cannot tell a dead stub from a deliberate cross-version
registration:

    java/lang/StringUTF16.isBigEndian()Z is absent from JDK 25 and PRESENT in
    JDK 17 and 21. native-builtins/src/lang_string.rs keeps it on purpose and
    says so in a 56-line comment.

`H25-1` N1 states the precondition in one sentence: run the multi-image sweep
FIRST; it is a precondition, not a follow-up. This script is that sweep.

WHAT IT DOES, AND WHAT IT REFUSES TO DO
---------------------------------------

For every registration in a `--dump-native-registry` JSON it asks each image
`javap -p -s --system <image>`: does this class declare this exact descriptor,
does a supertype declare it, or does neither. It then partitions the population:

  * LIVE      -- declared (or inherited) by EVERY image. Nothing to say.
  * PARTIAL   -- declared by SOME image only. The `isBigEndian` shape: a
                 registration that is correct precisely because another
                 supported image declares the method. NEVER retire one of
                 these on the strength of a single-image census.
  * NEAR_MISS -- some image declares the NAME on the class but no overload with
                 this descriptor. `H25-1` 2.2's population: an interception
                 somebody intended that has never once executed.
  * DEAD_EVERYWHERE -- no image declares it anywhere on the hierarchy, and the
                 class itself is present somewhere. The retirable population.
  * CLASS_ABSENT_EVERYWHERE -- the receiver class is on no image. Already
                 covered at class granularity by NO_IMAGE_JDK_RECEIVERS.

It reports. It does not edit, and it does not exit non-zero on a finding --
retiring a row is a source change with a duplicate-registration hazard
(`H22`, and trap 4 of the worker briefs: retiring the winner PROMOTES the loser)
that no census can see.

Usage:
    python3 scripts/jdk-only-no-image-methods.py \
        --registry reg-jdkonly.json \
        --images /data/jdkimages/jdk17-linux/jdk-17.0.20.1+1 \
                 /data/jdkimages/jdk21-linux/jdk-21.0.12+8 \
                 /data/jdkimages/jdk25-linux/jdk-25.0.4+7 \
        [--file-prefix native-builtins/src/lang_] [--csv out.csv]

An image path may be a JDK home or a macOS bundle root (`Contents/Home` is
appended when the bundle layout is detected).

Exit codes:
    0  the sweep ran (findings are printed, never fatal)
    2  refused to adjudicate -- unusable registry or image
    3  a prerequisite is missing (no javap)
"""
import argparse
import collections
import json
import os
import re
import shutil
import subprocess
import sys

CLASS_DECL_RE = re.compile(
    r"^(?:[\w@$.]+\s+)*?(?:class|interface|enum|record)\s+([\w.$]+)"
    r"(?:<[^{]*?>)?\s*(?:extends\s+([\w.$,<>\s]+?))?\s*(?:implements\s+([\w.$,<>\s]+?))?\s*\{",
    re.M,
)
DESC_RE = re.compile(r"^\s*descriptor:\s*(\S+)\s*$")


def resolve_image(path):
    """A JDK home, or a macOS bundle root whose home is Contents/Home."""
    if os.path.isdir(os.path.join(path, "Contents", "Home", "lib")):
        return os.path.join(path, "Contents", "Home")
    return path


def image_label(path):
    """A short, stable name for an image: the two path components that differ."""
    parts = [p for p in path.replace("\\", "/").split("/") if p]
    for i, p in enumerate(parts):
        if p.startswith("jdk") and "-" in p:
            return "/".join(parts[i:i + 2])
    return "/".join(parts[-2:])


def member_name(sig):
    """`public static java.lang.String valueOf(int);` -> `valueOf`.

    A constructor prints as the dotted class name with no return type; the
    registry spells it `<init>`. A field prints with no parentheses.
    """
    sig = sig.rstrip(";").strip()
    if "(" not in sig:
        toks = sig.split()
        return toks[-1] if toks else ""
    head = sig[: sig.index("(")]
    toks = head.split()
    tok = toks[-1] if toks else ""
    if "." in tok:
        return "<init>"
    return tok


class Image:
    """One JDK image, queried through `javap --system` and memoised per class."""

    def __init__(self, javap, home, label):
        self.javap = javap
        self.home = home
        self.label = label
        self._cache = {}

    def classinfo(self, internal_name):
        """-> (declared: set[(name, descriptor)], supertypes: list[internal]) or None."""
        if internal_name in self._cache:
            return self._cache[internal_name]
        dotted = internal_name.replace("/", ".")
        try:
            out = subprocess.run(
                [self.javap, "-p", "-s", "--system", self.home, dotted],
                capture_output=True, text=True, timeout=180,
            )
        except (OSError, subprocess.TimeoutExpired):
            self._cache[internal_name] = None
            return None
        if out.returncode != 0 or out.stdout.strip().startswith("Error:"):
            self._cache[internal_name] = None
            return None
        info = self._parse(out.stdout)
        self._cache[internal_name] = info
        return info

    @staticmethod
    def _parse(text):
        # A member line is followed by its own `descriptor:` line. Pair them
        # positionally rather than by name -- overloads share a name and only
        # the descriptor separates them.
        declared = set()
        pending = None
        for line in text.splitlines():
            m = DESC_RE.match(line)
            if m:
                if pending is not None:
                    declared.add((pending, m.group(1)))
                    pending = None
                continue
            stripped = line.strip()
            if not stripped or stripped.startswith("Compiled from"):
                continue
            if stripped.endswith(";"):
                pending = member_name(stripped)
            else:
                pending = None
        supers = []
        m = CLASS_DECL_RE.search(text)
        if m:
            for grp in (m.group(2), m.group(3)):
                if not grp:
                    continue
                for tok in grp.split(","):
                    tok = re.sub(r"<.*?>", "", tok).strip()
                    if tok:
                        supers.append(tok.replace(".", "/"))
        # javap prints no `extends` for a class whose direct superclass IS
        # Object, and an interface prints none at all -- but Object's methods
        # are inherited (a class) or implicitly declared abstract (an
        # interface) either way. Leaving Object out of the walk is what made an
        # earlier run of this script report `java/lang/Package.equals` as
        # declared by NO image, when every image declares it on Object and the
        # registration is a DELIBERATE override of it (H25-2 3.3).
        if "java/lang/Object" not in supers:
            supers.append("java/lang/Object")
        return declared, supers

    def declares(self, cls, name, desc, _depth=0):
        """DECLARED | INHERITED:<class> | NEAR_MISS | METHOD_ABSENT | CLASS_ABSENT"""
        info = self.classinfo(cls)
        if info is None:
            return "CLASS_ABSENT"
        declared, supers = info
        if (name, desc) in declared:
            return "DECLARED"
        if _depth < 8 and cls != "java/lang/Object":
            for sup in supers:
                r = self.declares(sup, name, desc, _depth + 1)
                if r == "DECLARED" or r.startswith("INHERITED:"):
                    return "INHERITED:" + sup
        if any(n == name for n, _ in declared):
            return "NEAR_MISS"
        return "METHOD_ABSENT"


SELFTEST_JAVAP = """Compiled from "AbstractStringBuilder.java"
abstract class java.lang.AbstractStringBuilder implements java.lang.Appendable, java.lang.CharSequence {
  byte[] value;
    descriptor: [B
  byte coder;
    descriptor: B
  boolean maybeLatin1;
    descriptor: Z
  int count;
    descriptor: I
  java.lang.AbstractStringBuilder(int);
    descriptor: (I)V
  public abstract java.lang.String toString();
    descriptor: ()Ljava/lang/String;
  private java.lang.AbstractStringBuilder repeat(char, int);
    descriptor: (CI)Ljava/lang/AbstractStringBuilder;
  public java.lang.AbstractStringBuilder repeat(java.lang.CharSequence, int);
    descriptor: (Ljava/lang/CharSequence;I)Ljava/lang/AbstractStringBuilder;
}
"""


def selftest():
    """The parse is the risky half of this script, so it is pinned.

    Two properties the sweep depends on and a regex could plausibly lose:
    overloads must be separated by DESCRIPTOR (not collapsed by name), and a
    constructor must come back as `<init>` because that is how the registry
    spells it.
    """
    declared, supers = Image._parse(SELFTEST_JAVAP)
    failures = []

    def want(cond, msg):
        if not cond:
            failures.append(msg)

    want(("count", "I") in declared, "the `count` field was not parsed")
    want(("value", "[B") in declared, "the `value` field was not parsed")
    want(("toString", "()Ljava/lang/String;") in declared, "abstract toString was not parsed")
    want(("<init>", "(I)V") in declared, "the constructor did not come back as <init>")
    want(("repeat", "(CI)Ljava/lang/AbstractStringBuilder;") in declared,
         "the private repeat(char,int) overload was lost")
    want(("repeat", "(Ljava/lang/CharSequence;I)Ljava/lang/AbstractStringBuilder;") in declared,
         "the repeat(CharSequence,int) overload was lost")
    want(("repeat", "(Ljava/lang/String;I)Ljava/lang/AbstractStringBuilder;") not in declared,
         "a descriptor NO image declares was reported as declared -- the parse is "
         "matching by name, which would make every near-miss look live")
    want("java/lang/Appendable" in supers and "java/lang/CharSequence" in supers,
         "the implements clause was not parsed: %r" % (supers,))
    want("java/lang/Object" in supers,
         "java/lang/Object must be walked even when javap prints no extends clause "
         "-- leaving it out reports every Object-inherited method as declared nowhere")

    for f in failures:
        print("SELFTEST FAIL: " + f, file=sys.stderr)
    if failures:
        return 2
    print("selftest: ok")
    return 0


def main():
    ap = argparse.ArgumentParser()
    if "--selftest" in sys.argv:
        return selftest()
    ap.add_argument("--registry", required=True)
    ap.add_argument("--images", nargs="+", required=True)
    ap.add_argument("--javap", default=None)
    ap.add_argument("--file-prefix", action="append", default=[],
                    help="only registrations whose registered_by starts with this")
    ap.add_argument("--class-prefix", action="append", default=[])
    ap.add_argument("--csv", default=None)
    ap.add_argument("--quiet", action="store_true")
    ap.add_argument("--selftest", action="store_true",
                    help="check the javap parse against a frozen sample and exit")
    ap.add_argument("--only-image-dead", action="store_true",
                    help="only rows the dump's own (single-image) adjudication "
                         "already calls undeclared -- the H25-1 342. Cheap way "
                         "to sweep a whole registry: a row the class-path image "
                         "declares needs no multi-image question asked.")
    args = ap.parse_args()

    javap = args.javap or shutil.which("javap")
    if not javap:
        print("REFUSING: no javap on PATH and none given with --javap.", file=sys.stderr)
        return 3
    try:
        with open(args.registry, encoding="utf-8") as fh:
            reg = json.load(fh)
    except (OSError, ValueError) as exc:
        print("REFUSING: cannot read %s: %s" % (args.registry, exc), file=sys.stderr)
        return 2
    nat = reg.get("natives")
    if not nat:
        print("REFUSING: %s has no `natives` array." % args.registry, file=sys.stderr)
        return 2

    images = []
    for p in args.images:
        home = resolve_image(p)
        if not os.path.isdir(os.path.join(home, "lib")):
            print("REFUSING: %s is not a JDK image (no lib/)." % p, file=sys.stderr)
            return 2
        images.append(Image(javap, home, image_label(p)))
    print("images: " + ", ".join(i.label for i in images))

    rows = []
    for r in nat:
        rb = r.get("registered_by") or ""
        if args.file_prefix and not any(rb.startswith(p) for p in args.file_prefix):
            continue
        if args.class_prefix and not any(r["class"].startswith(p) for p in args.class_prefix):
            continue
        if args.only_image_dead:
            im = r.get("image_declaring_method") or {}
            if im.get("declared") or im.get("inherited_from") or not im.get("image_has_class"):
                continue
        rows.append(r)
    print("registrations in scope: %d" % len(rows))

    out = []
    for r in rows:
        cls, name, desc = r["class"], r["name"], r["descriptor"]
        rb = r.get("registered_by") or ""
        verdicts = {im.label: im.declares(cls, name, desc) for im in images}
        live = [k for k, v in verdicts.items()
                if v == "DECLARED" or v.startswith("INHERITED:")]
        near = [k for k, v in verdicts.items() if v == "NEAR_MISS"]
        if live:
            bucket = "LIVE" if len(live) == len(images) else "PARTIAL"
        elif near:
            bucket = "NEAR_MISS"
        elif all(v == "CLASS_ABSENT" for v in verdicts.values()):
            bucket = "CLASS_ABSENT_EVERYWHERE"
        else:
            bucket = "DEAD_EVERYWHERE"
        out.append((bucket, cls, name, desc, r.get("owns_slot"), r.get("invocations"),
                    rb, ",".join(sorted(live)), verdicts))

    counts = collections.Counter(o[0] for o in out)
    print()
    for k in ("LIVE", "PARTIAL", "NEAR_MISS", "DEAD_EVERYWHERE", "CLASS_ABSENT_EVERYWHERE"):
        print("%-26s %5d" % (k, counts.get(k, 0)))

    if not args.quiet:
        for bucket in ("PARTIAL", "NEAR_MISS", "DEAD_EVERYWHERE"):
            sel = sorted(o for o in out if o[0] == bucket)
            if not sel:
                continue
            print()
            print("=== %s (%d) ===" % (bucket, len(sel)))
            for b, cls, name, desc, owns, inv, rb, live, verdicts in sel:
                print("  %s.%s%s" % (cls, name, desc))
                print("      owns_slot=%s invocations=%s @%s" % (owns, inv, rb))
                print("      " + "  ".join("%s=%s" % (k, v)
                                           for k, v in sorted(verdicts.items())))

    if args.csv:
        import csv as _csv
        with open(args.csv, "w", newline="", encoding="utf-8") as fh:
            w = _csv.writer(fh)
            w.writerow(["bucket", "class", "method", "descriptor", "owns_slot",
                        "invocations", "registered_by", "declared_by"]
                       + [im.label for im in images])
            for b, cls, name, desc, owns, inv, rb, live, verdicts in sorted(out):
                w.writerow([b, cls, name, desc, owns, inv, rb, live]
                           + [verdicts[im.label] for im in images])
        print()
        print("csv: %s" % args.csv)
    return 0


if __name__ == "__main__":
    sys.exit(main())
