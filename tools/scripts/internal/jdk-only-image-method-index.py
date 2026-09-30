#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Build the per-image METHOD index the multi-image sweep needs.

WHY THIS EXISTS
---------------

`scripts/jdk-only-no-image-receivers.py` answers "does any supported image
declare this receiver CLASS?".  `H25-1` measured 342 strict-mode registrations
that name a METHOD no JDK 25 image declares anywhere on the receiver's
hierarchy, and then corrected itself: `java/lang/StringUTF16.isBigEndian()Z` is
one of the 342 and is kept **deliberately**, with a 56-line comment in
`lang_string.rs` saying so, because **JDK 17 and 21 DO declare it**.

So "declared nowhere" measured on one image is not a work list.  Separating a
deliberate cross-version registration from a dead one needs the image bytes of
every supported (version, platform) pair, and that is what this script turns
into a file `scripts/jdk-only-no-image-methods.py` can read.

WHAT IT READS
-------------

A JDK image directory — anything containing `lib/modules` (the jimage), at any
of the three layouts Adoptium ships:

    <dir>/lib/modules                       (already a java.home)
    <dir>/jdk-21.0.12+8/lib/modules         (unpacked linux/windows archive)
    <dir>/jdk-21.0.12+8/Contents/Home/lib/modules   (unpacked macOS archive)

The jimage is read with the `jimage` tool of the JDK running this script, which
reads older images fine (MEASURED 2026-08-21: a JDK 25 `jimage` lists a JDK 21
*windows* image).  Class files are parsed here, in Python, down to the method
table only — `javap` on 28,000 classes is minutes per image and tells us
nothing extra.

WHAT IT WRITES
--------------

One gzipped JSON per image::

    {"label": "jdk21-windows", "release": 21, "platform": "windows",
     "java_version": "21.0.12+8", "class_count": 27884,
     "classes": {"java/lang/Thread": {"s": "java/lang/Object",
                                      "i": ["java/lang/Runnable"],
                                      "m": {"run()V": 1, "sleep0(J)V": 264}}}}

`m` maps `name+descriptor` to the method's `access_flags`, so a consumer can
tell a real `ACC_NATIVE` bridge target (0x0100) from ordinary bytecode without
re-reading the image.

Usage:
    python3 scripts/jdk-only-image-method-index.py \\
        --image /data/jdkimages/jdk21-windows --label jdk21-windows \\
        --out /data/jdkimages/index/jdk21-windows.json.gz

    python3 scripts/jdk-only-image-method-index.py --selftest

Exit codes:
    0  index written (or selftest passed)
    2  refused to index (no jimage found, jimage tool failed, empty result)
    3  selftest failed
"""
import argparse
import gzip
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile

# Constant-pool tags whose bodies are a fixed number of bytes after the tag.
# 1 (Utf8) is variable and 5/6 (Long/Double) additionally consume a second
# slot, so both are handled out of band in `parse_class`.
CP_FIXED = {
    3: 4, 4: 4, 7: 2, 8: 2, 9: 4, 10: 4, 11: 4, 12: 4,
    15: 3, 16: 2, 17: 4, 18: 4, 19: 2, 20: 2,
}
CP_WIDE = (5, 6)  # Long, Double — take two constant-pool slots (JVMS 4.4.5)


class BadClass(Exception):
    """This byte string is not a class file we can read to the method table."""


def parse_class(buf):
    """(name, super, [ifaces], {name+desc: acc}, {field: acc}) or raise BadClass.

    Deliberately a hand parse and not a library.  We need four fields and the
    method table; every class-file library in reach would either be a new
    dependency or `javap`, which costs minutes per image for the same answer.
    Attributes are SKIPPED by length, never decoded — a class file we cannot
    read past the method table is a `BadClass`, not a silent empty entry.
    """
    if len(buf) < 10 or buf[:4] != b"\xca\xfe\xba\xbe":
        raise BadClass("bad magic")
    p = 8
    (cp_count,) = struct.unpack_from(">H", buf, p)
    p += 2
    # Slot 0 is unused; `utf8` is sparse on purpose so a dangling index raises
    # KeyError here rather than reading as an empty name downstream.
    utf8 = {}
    cls_idx = {}
    i = 1
    while i < cp_count:
        tag = buf[p]
        p += 1
        if tag == 1:
            (n,) = struct.unpack_from(">H", buf, p)
            p += 2
            utf8[i] = buf[p:p + n].decode("utf-8", "replace")
            p += n
        elif tag in CP_WIDE:
            p += 8
            i += 1          # the wide entry eats the NEXT slot too
        elif tag == 7:
            (ni,) = struct.unpack_from(">H", buf, p)
            cls_idx[i] = ni
            p += 2
        elif tag in CP_FIXED:
            p += CP_FIXED[tag]
        else:
            raise BadClass("unknown constant-pool tag %d" % tag)
        i += 1

    def class_name(idx):
        if idx == 0:
            return None
        return utf8[cls_idx[idx]]

    p += 2  # access_flags
    (this_i, super_i, n_if) = struct.unpack_from(">HHH", buf, p)
    p += 6
    ifaces = []
    for _ in range(n_if):
        (ii,) = struct.unpack_from(">H", buf, p)
        p += 2
        ifaces.append(class_name(ii))

    def skip_members():
        nonlocal p
        (count,) = struct.unpack_from(">H", buf, p)
        p += 2
        out = []
        for _ in range(count):
            (acc, ni, di, nattr) = struct.unpack_from(">HHHH", buf, p)
            p += 8
            for _ in range(nattr):
                (_an, alen) = struct.unpack_from(">HI", buf, p)
                p += 6 + alen
            out.append((acc, utf8[ni], utf8[di]))
        return out

    fields = skip_members()
    methods = skip_members()
    # Field NAMES are kept, not descriptors: the consumer only asks "is this
    # registration's name a FIELD here rather than a method?", and WORKER 3's
    # sweep found 38 rows of that shape. An index built from the method table
    # alone must call every one of them dead.
    return (class_name(this_i), class_name(super_i), ifaces,
            {n + d: acc for (acc, n, d) in methods},
            {n: acc for (acc, n, _d) in fields})


def find_java_home(root):
    """The java.home inside an unpacked Adoptium archive, or None.

    Three layouts are accepted because the three archives unpack three ways;
    returning None rather than guessing is what makes the caller's REFUSE
    message point at a path instead of at an empty index.
    """
    cands = [root]
    try:
        cands += [os.path.join(root, e) for e in sorted(os.listdir(root))]
    except OSError:
        return None
    more = []
    for c in cands:
        more.append(os.path.join(c, "Contents", "Home"))
    for c in cands + more:
        if os.path.isfile(os.path.join(c, "lib", "modules")):
            return c
    return None


def read_release(java_home):
    """The image's own version string, from its `release` file. Best effort."""
    path = os.path.join(java_home, "release")
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                if line.startswith("JAVA_VERSION="):
                    return line.split("=", 1)[1].strip().strip('"')
    except OSError:
        pass
    return None


def extract(modules, into):
    """`jimage extract` the whole image. Returns the tool's stderr on failure."""
    jimage = shutil.which("jimage")
    if not jimage:
        return "no `jimage` on PATH"
    r = subprocess.run([jimage, "extract", "--dir", into, modules],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return (r.stderr or r.stdout or "rc=%d" % r.returncode).strip()[:400]
    return None


def index_tree(root):
    """Walk extracted class files into the index dict. Returns (classes, bad)."""
    classes = {}
    bad = 0
    for dirpath, _dirs, files in os.walk(root):
        for fn in files:
            if not fn.endswith(".class"):
                continue
            full = os.path.join(dirpath, fn)
            try:
                with open(full, "rb") as fh:
                    name, sup, ifaces, methods, fields = parse_class(fh.read())
            except (BadClass, KeyError, struct.error, OSError):
                bad += 1
                continue
            if not name or name in classes:
                # First module wins. A duplicate simple name across modules is
                # a patched/upgradeable module, which is not a shape any
                # registration in the census targets.
                continue
            classes[name] = {"s": sup, "i": [x for x in ifaces if x],
                             "m": methods, "f": fields}
    return classes, bad


def label_parts(label):
    """(release, platform) parsed out of a label like `jdk21-windows`."""
    m = re.match(r"jdk(\d+)[-_]([a-z0-9]+)", label.lower())
    if not m:
        return (None, None)
    plat = m.group(2)
    if plat.startswith("mac"):
        plat = "macos"
    if plat.startswith("win"):
        plat = "windows"
    return (int(m.group(1)), plat)


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--image", help="unpacked JDK image directory")
    ap.add_argument("--label", help="short name, e.g. jdk21-windows")
    ap.add_argument("--out", help="output path (.json.gz)")
    ap.add_argument("--keep-extract", metavar="DIR",
                    help="extract here and leave it (default: a temp dir, removed)")
    ap.add_argument("--selftest", action="store_true",
                    help="exercise the class parser and the layout finder; no JDK image")
    args = ap.parse_args(argv[1:])

    if args.selftest:
        return selftest()
    if not (args.image and args.label and args.out):
        sys.exit("REFUSING: --image, --label and --out are all required.")

    java_home = find_java_home(args.image)
    if not java_home:
        sys.exit("REFUSING: no `lib/modules` under %s (tried the dir itself, each"
                 " immediate child, and each child's Contents/Home). An image"
                 " that indexes to nothing agrees with every query." % args.image)
    modules = os.path.join(java_home, "lib", "modules")

    tmp = args.keep_extract or tempfile.mkdtemp(prefix="jdkimg-")
    made_tmp = args.keep_extract is None
    try:
        if not (os.path.isdir(tmp) and os.listdir(tmp)):
            err = extract(modules, tmp)
            if err:
                sys.exit("REFUSING: `jimage extract` failed on %s: %s"
                         % (modules, err))
        classes, bad = index_tree(tmp)
    finally:
        if made_tmp:
            shutil.rmtree(tmp, ignore_errors=True)

    if len(classes) < 1000:
        sys.exit("REFUSING: %s indexed to only %d classes. A real java.base is"
                 " ~6,000 on its own; this is a broken extract, and a thin index"
                 " would report every method as absent." % (args.label, len(classes)))

    release, platform = label_parts(args.label)
    doc = {
        "label": args.label,
        "release": release,
        "platform": platform,
        "java_version": read_release(java_home),
        "java_home": java_home,
        "class_count": len(classes),
        "unparsed_class_files": bad,
        "classes": classes,
    }
    os.makedirs(os.path.dirname(os.path.abspath(args.out)) or ".", exist_ok=True)
    opener = gzip.open if args.out.endswith(".gz") else open
    with opener(args.out, "wt", encoding="utf-8") as fh:
        json.dump(doc, fh, separators=(",", ":"))
    print("%s: %d classes, %d method rows, %d unparsed -> %s"
          % (args.label, len(classes),
             sum(len(c["m"]) for c in classes.values()), bad, args.out))
    return 0


def selftest():
    """Both halves of the parser, on bytes built here.

    A parser nobody has watched fail on a malformed input is decoration, so the
    negative cases are exercised too — including the two constant-pool shapes
    (Utf8's variable length, Long's stolen second slot) that a naive fixed-size
    loop gets wrong and that would misparse EVERY class rather than one.
    """
    def u1(x): return struct.pack(">B", x)
    def u2(x): return struct.pack(">H", x)
    def u4(x): return struct.pack(">I", x)
    def utf(s): return u1(1) + u2(len(s)) + s.encode()

    # cp: 1 Utf8 "Foo" 2 Utf8 "java/lang/Object" 3 Class->1 4 Class->2
    #     5 Long (eats 6) 7 Utf8 "sleep0" 8 Utf8 "(J)V" 9 Utf8 "Code"
    cp = (utf("Foo") + utf("java/lang/Object") + u1(7) + u2(1) + u1(7) + u2(2)
          + u1(5) + u4(0) + u4(7) + utf("sleep0") + utf("(J)V") + utf("Code"))
    body = (b"\xca\xfe\xba\xbe" + u2(0) + u2(65) + u2(10) + cp
            + u2(0x21) + u2(3) + u2(4) + u2(0)          # flags, this, super, 0 ifaces
            + u2(0)                                      # 0 fields
            + u2(1) + u2(0x0100) + u2(7) + u2(8) + u2(1)  # 1 method, ACC_NATIVE, 1 attr
            + u2(9) + u4(3) + b"\x00\x01\x02")            # the attribute, skipped by length
    name, sup, ifaces, methods, fields = parse_class(body)
    if (name, sup, ifaces) != ("Foo", "java/lang/Object", []):
        print("SELFTEST FAILED: header parsed as %r" % ((name, sup, ifaces),),
              file=sys.stderr)
        return 3
    if methods != {"sleep0(J)V": 0x0100}:
        print("SELFTEST FAILED: method table parsed as %r — the Long entry's"
              " second slot or the skipped attribute was mishandled" % methods,
              file=sys.stderr)
        return 3

    for label, blob in (("empty", b""),
                        ("bad magic", b"\x00\x00\x00\x00" + b"\x00" * 40),
                        ("truncated", body[:len(body) // 2])):
        try:
            parse_class(blob)
        except (BadClass, KeyError, struct.error, IndexError):
            pass
        else:
            print("SELFTEST FAILED: %s input parsed without raising" % label,
                  file=sys.stderr)
            return 3

    if label_parts("jdk21-windows") != (21, "windows"):
        print("SELFTEST FAILED: label_parts(jdk21-windows)", file=sys.stderr)
        return 3
    if label_parts("jdk17-mac-x64") != (17, "macos"):
        print("SELFTEST FAILED: label_parts(jdk17-mac-x64)", file=sys.stderr)
        return 3
    if label_parts("reg-linux25.json") != (None, None):
        print("SELFTEST FAILED: a name that is not jdk<N>-<plat> must parse to"
              " (None, None) so the sweep refuses it", file=sys.stderr)
        return 3

    with tempfile.TemporaryDirectory() as d:
        if find_java_home(d) is not None:
            print("SELFTEST FAILED: an empty dir must not look like a java.home",
                  file=sys.stderr)
            return 3
        deep = os.path.join(d, "jdk-21.0.12+8", "Contents", "Home", "lib")
        os.makedirs(deep)
        open(os.path.join(deep, "modules"), "wb").close()
        if find_java_home(d) != os.path.dirname(deep):
            print("SELFTEST FAILED: the macOS Contents/Home layout was not found",
                  file=sys.stderr)
            return 3

    print("selftest ok: header, the Utf8/Long constant-pool shapes, the"
          " skipped-attribute walk, three malformed inputs, label parsing and"
          " all three archive layouts.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
