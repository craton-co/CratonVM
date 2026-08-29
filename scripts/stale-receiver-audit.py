#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Find natives that keep using a receiver a moving collector may have moved.

WHY THIS EXISTS
---------------

`WORKER-5-NOTE-10` traced `RTreeRangeGc`'s compatible-mode failure to one
sentence of Rust:

    tm_sync_native_state(ctx, this)?;                  // ALLOCATES
    let size = tm_get_slot(ctx, this, TM_FIELD_SIZE);  // reads the OLD address

`tm_sync_native_state` rebuilds a view's storage — boxing keys, dispatching
`compareTo` into interpreted Java, allocating an array — so a moving collector
can relocate the receiver INSIDE the call. The caller kept its own copy. The
side table is keyed on `widened_obj_key(this)`, which starts with
`identity_hash_code(this)`; off a from-space address that reads a stale header,
lands in a different bucket, mints a FRESH slot, and the lookup answers the
DEFAULT. `TreeMap.size()` returned **0** on a view nothing had mutated.

Nothing crashes. That is what makes the shape worth a gate: it produces a
plausible wrong answer, and it needs a moving collection at one exact instant,
so it survives every green suite until an application trips it.

THE SHAPE, in three properties
------------------------------

  1. takes a receiver `ObjectRef` **by value**;
  2. can **allocate** (transitively, to `--depth`);
  3. returns `()` / `Result<(), _>` — it CANNOT hand the refreshed reference
     back, so callers are stuck with what they passed in.

(3) is the load-bearing one. A funnel that returns `ObjectRef` is safe by
construction: the caller cannot ignore the new value without ignoring the
result. That is why the fix for both known instances was a signature change —
`&mut ObjectRef` — rather than a pin at each call site: it makes every
unconverted caller a COMPILE ERROR instead of something a grep has to find
again next quarter.

A match is not a defect. The defect needs a CALL SITE that reuses the receiver
afterwards, so that is what the report counts.

WHAT IT DELIBERATELY DOES NOT DO
--------------------------------

It does not decide whether a site is exploitable — that needs a reproduction,
and the two fixed instances differ: the TreeMap one failed 100% under
`--Xmx 64m --nojit`, while `resync_view_set` (identical shape) could not be made
to fail at all. Both were still fixed. This script's job is to keep the
POPULATION from growing silently, not to rank it.

Usage:
    scripts/stale-receiver-audit.py                 # report + compare baseline
    scripts/stale-receiver-audit.py --update        # re-baseline
    scripts/stale-receiver-audit.py --detail        # every site, with first use
    scripts/stale-receiver-audit.py --selftest      # no tree needed
    scripts/stale-receiver-audit.py --depth 1       # narrow reachability
                                                    # (default 6 = the fixpoint)

Exit: 0 ok · 1 the population GREW · 2 no baseline · 3 the gate is broken
"""
import argparse
import io
import os
import re
import sys
import collections

CRATES = ["native-builtins", "native-collections", "native-io", "native-api",
          "native-builtins-crypto", "native-builtins-security", "native-awt"]

# Level 0: primitives that can move the heap. `ctx.invoke*` counts because it
# dispatches interpreted Java, which allocates freely.
ALLOC0 = re.compile(
    r"\balloc_ref_array\b|\btry_alloc_synthetic\b|\bctx\.alloc_object\b"
    r"|\bctx\.new_array\b|\bctx\.new_string\b|\bctx\.alloc_"
    r"|\bctx\.invoke[a-z_0-9]*\s*\("
    r"|\bctx\.ensure_class_initialized\b|\bctx\.intern\b|\bctx\.box_")
FNDEF = re.compile(r"^(pub(\([a-z ]+\))? )?(async )?(unsafe )?fn ([a-z_][a-z_0-9]*)")
CALL = re.compile(r"(?<![a-z_0-9.])([a-z_][a-z_0-9]*)\(\s*ctx\s*,\s*([a-z_][a-z_0-9]*)\s*\)")
# Every `name(` that is not a method call -- the callee edge of the call graph.
CALLEE = re.compile(r"(?<![a-z_0-9.])([a-z_][a-z_0-9]*)\s*\(")
RECV = re.compile(r"\b([a-z_][a-z_0-9]*): ObjectRef\b")

Fn = collections.namedtuple("Fn", "name file line sig code crate")


def strip_comments(lines):
    """CODE ONLY.

    Found the hard way: `seed_buffer_byte_order` was first reported as a
    DIRECTLY-allocating candidate because `ctx.alloc_object` appears in its DOC
    COMMENT. Three of the first 21 candidates were prose. A detector that reads
    comments as code manufactures exactly the false positives this exists to
    remove.
    """
    return "\n".join(l for l in lines if not l.strip().startswith(("//", "*", "/*")))


def index(root, crates):
    files = []
    for c in crates:
        base = os.path.join(root, c, "src")
        for dp, _d, ns in os.walk(base):
            files += [os.path.join(dp, n) for n in ns if n.endswith(".rs")]
    doc, fns = {}, []
    for f in files:
        lines = io.open(f, encoding="utf-8", errors="replace", newline="").read().split("\n")
        idx = [i for i, l in enumerate(lines) if FNDEF.match(l)]
        doc[f] = (lines, idx)
        rel = f.replace("\\", "/").replace(root.replace("\\", "/").rstrip("/") + "/", "")
        crate = rel.split("/")[0]
        for n, i in enumerate(idx):
            j = idx[n + 1] if n + 1 < len(idx) else len(lines)
            fns.append(Fn(FNDEF.match(lines[i]).group(5), rel, i + 1,
                          "\n".join(lines[i:j]).split("{")[0],
                          strip_comments(lines[i:j]), crate))
    return files, doc, fns


def allocating(fns, depth):
    """{fn name: the depth at which allocation was reached}.

    The callee names of each body are tokenised ONCE and reachability is a set
    intersection. The obvious spelling -- one `a|b|c|...` alternation per round
    over every body -- is quadratic in the frontier, and at `--depth 3` (a
    6,000-name frontier) it did not finish in ten minutes. `CALLEE` is the same
    pattern that alternation used, so the two agree; `--selftest` and the
    committed depth-1/depth-2 numbers are what hold them to that.
    """
    alloc = {fn.name: 0 for fn in fns if ALLOC0.search(fn.code)}
    callees = [(fn.name, set(CALLEE.findall(fn.code))) for fn in fns]
    for d in range(1, depth + 1):
        known = set(alloc)
        if not known:
            break
        add = {n: d for (n, cs) in callees if n not in alloc and not cs.isdisjoint(known)}
        if not add:
            break
        alloc.update(add)
    return alloc


def candidates(fns, alloc):
    out = {}
    for fn in fns:
        if not RECV.search(fn.sig) or ": &mut ObjectRef" in fn.sig:
            continue
        if not (("-> Result<(), MethodCallFailed>" in fn.sig) or ("->" not in fn.sig)):
            continue
        if fn.name in alloc:
            out[fn.name] = (fn, alloc[fn.name])
    return out


def reusing_sites(files, doc, cands, root):
    """Call sites that keep using the receiver after the call.

    A REBIND (`let x = ...`) or a `read_native_pin` re-read ENDS the window —
    both give the caller a fresh value, so neither is a stale use.
    """
    out = collections.defaultdict(list)
    for f in files:
        lines, idx = doc[f]
        rel = f.replace("\\", "/").replace(root.replace("\\", "/").rstrip("/") + "/", "")
        for i, l in enumerate(lines):
            if FNDEF.match(l):
                continue
            for m in CALL.finditer(l):
                name, var = m.group(1), m.group(2)
                if name not in cands:
                    continue
                end = next((s for s in idx if s > i), len(lines))
                after = []
                for k in range(i + 1, end):
                    st = lines[k].strip()
                    if st.startswith(("//", "*")):
                        continue
                    if not re.search(r"[^a-z_0-9.\"]" + re.escape(var) + r"[^a-z_0-9]", lines[k]):
                        continue
                    if re.match(r"let (mut )?" + re.escape(var) + r"\s*=", st):
                        break
                    if "read_native_pin" in st and var in st:
                        break
                    after.append((k + 1, st))
                if after:
                    out[name].append((rel, i + 1, var, after[0][0], after[0][1][:100]))
    return out


def run(root, depth):
    files, doc, fns = index(root, CRATES)
    alloc = allocating(fns, depth)
    cands = candidates(fns, alloc)
    sites = reusing_sites(files, doc, cands, root)
    # A call site names a FUNCTION and this tool has no module resolution, so
    # candidates are keyed by NAME. 66 of the tree's 16,347 native fn names are
    # defined in more than one place; for those the crate label is whichever
    # definition was indexed last, and the allocation verdict is the UNION over
    # them. Such rows print `AMBIG` rather than passing as precise -- it is why
    # a crate's count can move between --depth settings with no code change.
    defs = collections.Counter(fn.name for fn in fns)
    return files, fns, alloc, cands, sites, defs


def selftest():
    import tempfile, shutil
    t = tempfile.mkdtemp()
    try:
        d = os.path.join(t, "native-collections", "src")
        os.makedirs(d)
        io.open(os.path.join(d, "lib.rs"), "w", encoding="utf-8").write('''
fn allocs(ctx: &mut dyn NativeContext, this: ObjectRef) -> Result<(), MethodCallFailed> {
    let _ = ctx.alloc_object(0, 1);
    Ok(())
}
fn only_a_comment(ctx: &mut dyn NativeContext, this: ObjectRef) {
    // ctx.alloc_object is named here in PROSE only
    let _ = this;
}
fn returns_the_ref(ctx: &mut dyn NativeContext, this: ObjectRef) -> Result<ObjectRef, X> {
    let _ = ctx.alloc_object(0, 1);
    Ok(this)
}
fn caller_reuses(ctx: &mut dyn NativeContext, this: ObjectRef) {
    allocs(ctx, this);
    let _ = ctx.get_field(this, 0);
}
fn caller_rebinds(ctx: &mut dyn NativeContext, this: ObjectRef) {
    allocs(ctx, this);
    let this = obj_arg(args, 0);
    let _ = ctx.get_field(this, 0);
}
fn caller_repins(ctx: &mut dyn NativeContext, this: ObjectRef) {
    allocs(ctx, this);
    let this = ctx.read_native_pin(pin, this);
    let _ = ctx.get_field(this, 0);
}
fn caller_only_comments(ctx: &mut dyn NativeContext, this: ObjectRef) {
    allocs(ctx, this);
    // this is mentioned only in prose
}
''')
        _f, _fns, alloc, cands, sites, _defs = run(t, 1)
        fails = 0

        def ck(cond, what):
            nonlocal fails
            print("  %s   %s" % ("ok  " if cond else "FAIL", what))
            if not cond:
                fails = 1

        ck("allocs" in alloc and alloc["allocs"] == 0, "a direct allocator is depth 0")
        ck("only_a_comment" not in alloc,
           "ctx.alloc_object in a COMMENT is not an allocation")
        ck("allocs" in cands, "receiver-by-value + allocates + returns unit is a candidate")
        ck("returns_the_ref" not in cands,
           "a funnel that RETURNS the ref is not a candidate")
        ck("only_a_comment" not in cands, "a non-allocator is not a candidate")
        got = {s[0].split("/")[-1] + ":" + str(s[1]) for s in sites.get("allocs", [])}
        lines = [s[1] for s in sites.get("allocs", [])]
        srclines = io.open(os.path.join(d, "lib.rs"), encoding="utf-8").read().split("\n")
        callers = {srclines[l - 1].strip(): True for l in lines}
        # exactly ONE of the four callers reuses the receiver
        ck(len(sites.get("allocs", [])) == 1,
           "exactly one of four callers counts (reuse yes; rebind, re-pin and comment no)"
           "  got %d" % len(sites.get("allocs", [])))
        print("  selftest %s" % ("OK" if not fails else "FAILED"))
        return fails
    finally:
        shutil.rmtree(t, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    # 6 is the FIXPOINT: `allocating` stops growing there (5147 at depth 1,
    # 6104 at 2, 6348 at 3, 6476 at 4, 6538 at 5, 6538 at 6 and at 12), so
    # the default is a converged answer rather than an arbitrary cut. It is
    # not a slow one -- the whole scan is ~9s.
    ap.add_argument("--depth", type=int, default=6)
    ap.add_argument("--update", action="store_true")
    ap.add_argument("--detail", action="store_true")
    ap.add_argument("--selftest", action="store_true")
    ap.add_argument("--baseline",
                    default=os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                         "baselines", "stale-receiver-sites.txt"))
    a = ap.parse_args()
    if a.selftest:
        sys.exit(3 if selftest() else 0)

    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    files, fns, alloc, cands, sites, defs = run(root, a.depth)
    total = sum(len(v) for v in sites.values())
    print("STALE-RECEIVER AUDIT (allocation reachability depth <= %d)" % a.depth)
    print("  files %d   fns %d   allocating %d   matching the shape %d"
          % (len(files), len(fns), len(alloc), len(cands)))
    print("  WITH call sites that reuse the receiver: %d fn(s), %d site(s)"
          % (len(sites), total))
    print("  A match is not a defect — see the header. This gate watches the")
    print("  population, it does not rank it.")
    rows = sorted(sites.items(), key=lambda kv: (-len(kv[1]), kv[0]))
    for name, ss in rows:
        fn, d = cands[name]
        amb = "  AMBIG(%d defs)" % defs[name] if defs[name] > 1 else ""
        print("    %-44s %-24s depth=%d sites=%d%s"
              % (name, fn.crate, d, len(ss), amb))
        if a.detail:
            for s in ss:
                print("        %s:%d recv=%s -> first use @%d: %s"
                      % (s[0], s[1], s[2], s[3], s[4]))

    key = sorted("%s %d" % (n, len(s)) for n, s in sites.items())
    if a.update:
        os.makedirs(os.path.dirname(a.baseline), exist_ok=True)
        with io.open(a.baseline, "w", encoding="utf-8", newline="\n") as fh:
            fh.write("# stale-receiver audit baseline — scripts/stale-receiver-audit.py\n")
            fh.write("# fn <sites-that-reuse-the-receiver>. See the script header for the\n")
            fh.write("# shape and for why a match is not automatically a defect.\n")
            fh.write("depth=%d\n" % a.depth)
            for k in key:
                fh.write(k + "\n")
        print("  baseline written: %d fn(s), %d site(s)" % (len(sites), total))
        return 0
    if not os.path.exists(a.baseline):
        print("  NO BASELINE at %s — run --update" % a.baseline)
        return 2
    base = [l.strip() for l in io.open(a.baseline, encoding="utf-8")
            if l.strip() and not l.startswith("#") and not l.startswith("depth=")]
    bset, nset = dict(x.rsplit(" ", 1) for x in base), dict(x.rsplit(" ", 1) for x in key)
    grew = [n for n in nset if n not in bset or int(nset[n]) > int(bset[n])]
    gone = [n for n in bset if n not in nset]
    for n in gone:
        print("  IMPROVED: %s no longer has reusing call sites" % n)
    if grew:
        for n in grew:
            print("  TRIPPED: %s now has %s reusing site(s) (baseline %s)"
                  % (n, nset[n], bset.get(n, "0")))
        print("  A new one is a native that allocates and then keeps using its")
        print("  receiver. Give the funnel a `&mut ObjectRef` receiver, which makes")
        print("  every unconverted caller a compile error — see WORKER-5-NOTE-10 §7.3.")
        return 1
    print("  ok — no new stale-receiver sites.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
