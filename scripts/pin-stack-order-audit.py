#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Find a native that reads a pin it has already released by releasing an OLDER one.

WHY THIS EXISTS
---------------

`NativeContext::unpin_native_roots(base)` TRUNCATES the thread's pin stack at
`base`: it releases the pin `base` names AND every pin pushed after it. So

    let a_pin = ctx.pin_native_root(a);
    let b_pin = ctx.pin_native_root(b);
    ...
    ctx.unpin_native_roots(a_pin);          // also releases b_pin
    ... allocate or call Java ...
    let b = ctx.read_native_pin(b_pin, b);  // no entry: answers the fallback

compiles, runs, and returns `b`'s address from BEFORE the collection.
`read_native_pin` falls back silently when the handle is past the end of the
stack, so nothing warns. `nio_file.rs::p57_alloc_default_filesystem_concrete`
had exactly this shape. Under ZGC stress it wrote the default FileSystem's
`defaultDirectory` / `defaultRoot` into a vacated span. That was the
`zgc mark: skipped non-registered child pointers` flood of
`common-w20v-zgc-mark-skips-wild-child-pointers-under-nio-selector-stress`,
one run in five, for at least four waves.

The rule: release pins in the reverse order you took them, or release the
OLDEST one once, after the last read of any of them.

WHAT IT CHECKS, AND WHAT IT DOES NOT
------------------------------------

Per function, in source order: `let x = ..pin_native_root(..)` pushes `x`. An
`unpin_native_roots(a)` at the pin's own indentation or shallower releases `a`
and every pin pushed after it. An unpin nested deeper than its pin is taken to
be an early-exit branch and is ignored. A later `read_native_pin(v, ..)` of a
released `v` is a site. A nested `fn` starts a fresh pin stack. The scan does
not follow calls: a helper that unpins its caller's base is invisible.

The baseline is EMPTY: any site fails the job.

Usage:
    scripts/pin-stack-order-audit.py             # scan; exit 1 on any site
    scripts/pin-stack-order-audit.py --selftest  # no tree needed
"""
import argparse
import importlib.util
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location(
    "stale_handle_audit", os.path.join(HERE, "stale-handle-across-alloc-audit.py"))
audit = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(audit)

PIN = re.compile(r"\blet\s+(?:mut\s+)?(\w+)\s*(?::\s*usize\s*)?=\s*(?:[\w.*&]+\.)?pin_native_root\s*\(")
UNPIN = re.compile(r"\bunpin_native_roots\s*\(\s*(\w+)\s*\)")
READ = re.compile(r"\bread_native_pin\s*\(\s*(\w+)\s*,")


def scan_lines(lines, path="<mem>"):
    hits = []
    tests = audit.cfg_test_lines(lines)
    for name, start, body in audit.functions(lines):
        order, indent, released, seq = {}, {}, {}, 0
        for k, raw in enumerate(body):
            code = audit.strip_code(raw)
            if k > 0 and audit.FNDEF.match(raw):
                order, indent, released, seq = {}, {}, {}, 0
            depth = len(raw) - len(raw.lstrip())
            m = PIN.search(code)
            if m:
                seq += 1
                order[m.group(1)] = seq
                indent[m.group(1)] = depth
                released.pop(m.group(1), None)
            for m in UNPIN.finditer(code):
                a = m.group(1)
                if a in order and depth <= indent[a]:
                    for v, o in order.items():
                        if o >= order[a]:
                            released.setdefault(v, k)
            for m in READ.finditer(code):
                v = m.group(1)
                if v in released and released[v] < k:
                    ln = start + k + 1
                    if ln not in tests:
                        hits.append((path, ln, name, v, start + released[v] + 1))
    return hits


def selftest():
    bad = """
fn build(ctx: &mut dyn NativeContext, a: ObjectRef, b: ObjectRef) -> ObjectRef {
    let a_pin = ctx.pin_native_root(a);
    let b_pin = ctx.pin_native_root(b);
    ctx.unpin_native_roots(a_pin);
    let s = ctx.create_string("x");
    let b = ctx.read_native_pin(b_pin, b);
    b
}
""".splitlines()
    good = """
fn build(ctx: &mut dyn NativeContext, a: ObjectRef, b: ObjectRef) -> ObjectRef {
    let a_pin = ctx.pin_native_root(a);
    let b_pin = ctx.pin_native_root(b);
    if a.is_null() {
        ctx.unpin_native_roots(a_pin);
        return a;
    }
    let s = ctx.create_string("x");
    let b = ctx.read_native_pin(b_pin, b);
    ctx.unpin_native_roots(a_pin);
    b
}
""".splitlines()
    broken = 0
    if len(scan_lines(bad)) != 1:
        print("  the detector missed the canonical shape: %r" % (scan_lines(bad),))
        broken += 1
    if scan_lines(good):
        print("  the detector flagged the correct shape: %r" % (scan_lines(good),))
        broken += 1
    print("  selftest: %s" % ("BROKEN" if broken else "ok"))
    return 3 if broken else 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=os.path.dirname(HERE))
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    rc = selftest()
    if a.selftest or rc:
        return rc
    hits = []
    for crate in audit.CRATES:
        base = os.path.join(a.root, crate, "src")
        for dirpath, _, files in os.walk(base):
            for f in sorted(files):
                if not f.endswith(".rs"):
                    continue
                p = os.path.join(dirpath, f)
                rel = os.path.relpath(p, a.root).replace("\\", "/")
                lines = open(p, encoding="utf-8", errors="replace").read().splitlines()
                hits += scan_lines(lines, rel)
    for path, ln, name, v, at in hits:
        print("  %s:%d %s: read_native_pin(%s) after its pin was released at line %d"
              % (path, ln, name, v, at))
    if hits:
        print("  %d site(s). Release pins newest-first, or release the oldest"
              " once after the last read." % len(hits))
        return 1
    print("  ok -- no pin is read after an older pin released it")
    return 0


if __name__ == "__main__":
    sys.exit(main())
