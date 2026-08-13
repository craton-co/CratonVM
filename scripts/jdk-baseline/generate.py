#!/usr/bin/env python3
"""Generate / check the checked-in JDK surface baselines in scripts/baselines/.

    python scripts/jdk-baseline/generate.py            # --check (read-only, the CI default)
    python scripts/jdk-baseline/generate.py --update   # rewrite the baselines
    python scripts/jdk-baseline/generate.py --verify    # generator known-answer test only

Why this exists
---------------
docs/known-issues/jdk-only/E25-R11-GUARD-POPULATION-SWEEP-20260813.md §7:

    "Across `native-builtins/src/` -- 30 guards of this shape -- not one test
     reads a checked-in baseline file, and not one invokes `javap`. Every
     expected set in the crate was typed by a person reading the code beside
     it. That is not thirty independent lapses; it is one missing capability,
     thirty times."

This is that capability, as data plus a generator. The consuming side (a Rust
helper and the rewritten guards) is nominated, not written, in
docs/known-issues/jdk-only/E32-R11-JDK-BASELINE-CAPABILITY-20260813.md.

The oracle is JdkBaseline.java in this directory, which reads
`jrt:/modules/<module>/<binary/name>.class` out of the running JDK's own image
and parses it with `java.lang.classfile`. This script only orchestrates it:
compile-cache, write, diff, and the known-answer test.

Exit codes
----------
    0  everything agreed
    1  a baseline drifted (see the classification in the output)
    2  the environment is wrong (no java/javac, compile failed, bad JDK)
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
OUT_DIR = REPO / "scripts" / "baselines"
CLASSES = HERE / "classes.txt"
JAVA_SRC = HERE / "JdkBaseline.java"
PREFIX = "jdk25-"

# ---------------------------------------------------------------------------
# (c) The generator's known-answer test.
#
# These two numbers are NOT a completeness guard and must never be read as one.
# They are a KAT for the instrument: two counts measured independently, by hand,
# with `javap` on openjdk 25.0.3+9 (Microsoft-13877124) on 2026-08-13, before
# this generator existed. If the generator disagrees with either, the generator
# is wrong -- it is over- or under-counting bridge methods, constructors, or
# synthetic members -- and every baseline it wrote is suspect.
#
#   $ javap -public javax.crypto.Mac  | grep -c '('        -> 17   (no public ctor)
#   $ javap -public java.lang.Character | grep -c '('      -> 97
#   $ javap -public java.lang.Character | grep -c 'Character('  -> 1  (the deprecated ctor)
#                                                          96 = 97 - 1
#
# The Character figure INCLUDES the bridge `compareTo(Ljava/lang/Object;)I`, and
# that is deliberate: the bridge is a real entry in the class file's method
# table, and it is a real dispatch target the VM must answer for. A generator
# that filtered bridges would report 95 and quietly disagree with `javap`.
# ---------------------------------------------------------------------------
KNOWN_ANSWERS = {
    "javax.crypto.Mac": 17,
    "java.lang.Character": 96,
}

# Header keys whose value is a property of the JDK BUILD, not of the class
# surface. A diff confined to these means "the JDK moved"; a diff anywhere else
# means "the surface moved". The two need different human responses, so --check
# reports them separately instead of printing one undifferentiated red.
JDK_IDENTITY_KEYS = ("java.version", "java.vm.version", "java.vendor.version")


def die(msg: str, code: int = 2) -> None:
    print(f"generate.py: {msg}", file=sys.stderr)
    sys.exit(code)


def tool(name: str) -> str:
    p = shutil.which(name)
    if not p:
        die(f"{name} is not on PATH. This generator needs a JDK 25 (or later) "
            f"toolchain: java.lang.classfile is final from JDK 24.")
    return p


def compile_cached() -> Path:
    """Compile JdkBaseline.java once per source revision, outside the repo.

    Single-file source mode (`java JdkBaseline.java`) recompiles on every run --
    measured at ~50 s on the Windows host this was written on -- and it would be
    the whole cost of a --check. Build output must not land in the repo, so the
    cache lives in the system temp keyed by the source hash: editing the source
    invalidates it, and nothing stale can be picked up.
    """
    src = JAVA_SRC.read_bytes()
    key = hashlib.sha256(src).hexdigest()[:16]
    cache = Path(tempfile.gettempdir()) / f"jdk-baseline-{key}"
    stamp = cache / "JdkBaseline.class"
    if stamp.exists():
        return cache
    cache.mkdir(parents=True, exist_ok=True)
    r = subprocess.run([tool("javac"), "-d", str(cache), str(JAVA_SRC)],
                       capture_output=True, text=True)
    if r.returncode != 0:
        shutil.rmtree(cache, ignore_errors=True)
        die(f"javac failed:\n{r.stdout}\n{r.stderr}")
    return cache


def run_generator(dest: Path) -> None:
    cache = compile_cached()
    r = subprocess.run(
        [tool("java"), "-cp", str(cache), "JdkBaseline",
         "--classes-file", str(CLASSES), "--out", str(dest), "--prefix", PREFIX],
        capture_output=True, text=True)
    if r.returncode != 0:
        die(f"JdkBaseline failed:\n{r.stdout}\n{r.stderr}")


def norm(b: bytes) -> list[str]:
    """CRLF-insensitive line view.

    `.gitattributes` does not pin `scripts/baselines/*.tsv` to LF and this repo
    is developed with core.autocrlf=true, so the checked-out file is CRLF on
    Windows and LF on Linux while the generator always writes LF. Comparing raw
    bytes would make --check fail on one platform and pass on the other, for a
    difference that is not in the data. See NOM E32-1 in the record: the real
    fix is a .gitattributes line, and until it lands EVERY reader of these files
    -- this script and the nominated Rust helper both -- must trim_end().
    """
    return b.decode("utf-8").replace("\r\n", "\n").split("\n")


def split_header(lines: list[str]) -> tuple[dict[str, str], list[str]]:
    hdr, body = {}, []
    for ln in lines:
        if ln.startswith("# "):
            k, _, v = ln[2:].partition("\t")
            hdr[k] = v
        elif ln.strip():
            body.append(ln)
    return hdr, body


def verify_known_answers(fresh: Path) -> int:
    """(c): the generator must reproduce two independently measured counts."""
    bad = 0
    print("--- generator known-answer test (see KNOWN_ANSWERS for provenance) ---")
    for cls, expected in sorted(KNOWN_ANSWERS.items()):
        f = fresh / f"{PREFIX}{cls}.tsv"
        if not f.exists():
            print(f"  FAIL {cls}: the generator emitted no baseline at all")
            bad += 1
            continue
        hdr, body = split_header(norm(f.read_bytes()))
        declared = int(hdr["public-methods"])
        # Recount from the rows rather than trusting the header the same
        # program wrote: a header count that restates its own writer is the
        # exact shape E25 is about.
        counted = sum(
            1 for r in body
            if r.startswith("METHOD\t")
            and r.split("\t")[1] not in ("<init>", "<clinit>")
            and "public" in r.split("\t")[3].split(","))
        ok = declared == expected == counted
        print(f"  {'OK  ' if ok else 'FAIL'} {cls}: expected {expected}, "
              f"header says {declared}, rows count {counted}")
        if not ok:
            bad += 1
    if bad:
        print("\nThe GENERATOR is wrong, not the JDK. Do not update the "
              "baselines from a generator that fails its own known answers.")
    return bad


def check(fresh: Path) -> int:
    """Two-way, file-level ratchet: a new file, a missing file, or any changed row."""
    have = {p.name for p in OUT_DIR.glob(f"{PREFIX}*.tsv")}
    want = {p.name for p in fresh.glob(f"{PREFIX}*.tsv")}
    bad = 0

    for name in sorted(want - have):
        print(f"MISSING  {name}: the generator emits this baseline and it is not "
              f"checked in. Run --update.")
        bad += 1
    for name in sorted(have - want):
        print(f"STALE    {name}: checked in, but nothing in classes.txt produces "
              f"it. Either restore the classes.txt entry or delete the file -- a "
              f"baseline no generator writes can never go red again.")
        bad += 1

    for name in sorted(have & want):
        old_h, old_b = split_header(norm((OUT_DIR / name).read_bytes()))
        new_h, new_b = split_header(norm((fresh / name).read_bytes()))
        if old_b == new_b and old_h == new_h:
            continue
        bad += 1
        if old_b == new_b:
            moved = [k for k in JDK_IDENTITY_KEYS if old_h.get(k) != new_h.get(k)]
            if moved and all(old_h.get(k) == new_h.get(k)
                             for k in old_h.keys() | new_h.keys()
                             if k not in JDK_IDENTITY_KEYS):
                print(f"JDK-BUMP {name}: the surface is byte-identical; only "
                      f"{', '.join(moved)} moved "
                      f"({', '.join(f'{k}: {old_h.get(k)!r} -> {new_h.get(k)!r}' for k in moved)}). "
                      f"Re-run --update and say so in the commit message.")
                continue
        print(f"DRIFT    {name}:")
        for r in sorted(set(new_b) - set(old_b)):
            print(f"           + {r}")
        for r in sorted(set(old_b) - set(new_b)):
            print(f"           - {r}")
        for k in sorted(old_h.keys() | new_h.keys()):
            if old_h.get(k) != new_h.get(k):
                print(f"           ~ # {k}: {old_h.get(k)!r} -> {new_h.get(k)!r}")

    if not bad:
        print(f"OK: {len(have)} baselines in {OUT_DIR.relative_to(REPO)} agree "
              f"with this JDK.")
    return bad


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--update", action="store_true",
                    help="rewrite scripts/baselines/jdk25-*.tsv from this JDK")
    ap.add_argument("--check", action="store_true",
                    help="(default) regenerate to a temp dir and diff; never writes")
    ap.add_argument("--verify", action="store_true",
                    help="run only the generator known-answer test")
    args = ap.parse_args()

    if not CLASSES.exists():
        die(f"{CLASSES} is missing")

    with tempfile.TemporaryDirectory(prefix="jdk-baseline-out-") as td:
        fresh = Path(td)
        run_generator(fresh)

        # The KAT runs in EVERY mode, including --update. A generator that
        # cannot reproduce two measured counts must not be allowed to overwrite
        # a baseline that other tests will then treat as the JDK's own answer.
        if verify_known_answers(fresh):
            return 1
        if args.verify:
            return 0

        if args.update:
            OUT_DIR.mkdir(parents=True, exist_ok=True)
            for p in sorted(fresh.glob(f"{PREFIX}*.tsv")):
                (OUT_DIR / p.name).write_bytes(p.read_bytes())
                print(f"wrote scripts/baselines/{p.name}")
            # Stale files are reported, never deleted: removing a baseline is a
            # deliberate act, and a silent delete would take a guard's
            # population with it.
            for p in sorted(OUT_DIR.glob(f"{PREFIX}*.tsv")):
                if not (fresh / p.name).exists():
                    print(f"STALE (not deleted) scripts/baselines/{p.name}: "
                          f"nothing in classes.txt produces it.")
            return 0

        return 1 if check(fresh) else 0


if __name__ == "__main__":
    sys.exit(main())
