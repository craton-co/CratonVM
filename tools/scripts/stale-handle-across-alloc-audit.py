#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Find a native that keeps USING an ObjectRef it read before an allocation.

WHY THIS EXISTS, AND WHY IT IS NOT `stale-receiver-audit.py`
------------------------------------------------------------

`scripts/stale-receiver-audit.py` screens a CALLEE shape: a helper that takes a
receiver by value, can allocate, and returns `()`, so its callers are stuck with
the address they passed in. That gate is real and its baseline is empty.

It cannot see the shape this one screens, which is entirely INSIDE one function:

    let arr = ctx.new_ref_array(cid, n);          // young object
    for i in 0..n {
        let e = try_alloc_concurrent_synthetic(ctx, ...)?;   // ← can collect
        ctx.set_array_element(arr, i, Value::Object(Some(e)));
        //                    ^^^ the address `new_ref_array` returned, which
        //                        the collection above may have vacated
    }

Nothing crashes. The store lands in the pre-move copy, the surviving array keeps
`null` in that slot, and every `[deadref-*]` value-side probe is silent because
the VALUE stored (`e`) is live — it is the RECEIVER that is dead. That is the
same species as
`docs/internal/springboot/bindabletests-assertj-objects-receiver-stale-across-clinit-20260911.md`,
whose one defect cost a week precisely because no screen looked at the receiver.

The rule the shape violates: **a pin is a fact about an object, not about a
variable.** `pin_native_root`/`handle_root` root the OBJECT; only
`read_native_pin`/`NativeHandleScope::get` produce an address that is valid NOW.

THE SHAPE, in three properties
------------------------------

  1. a local (or argument-derived) `ObjectRef` bound at some point in a body;
  2. a call that can allocate or run Java between that binding and a later use;
  3. that later use puts it in a RECEIVER position — `set_field`,
     `get_field`, `set_array_element`, `get_array_element`, `read_string`, an
     `invoke_virtual`, and so on.

A match is not a defect: the receiver may be an old-generation singleton, the
"allocating" call may be on a path that cannot allocate, and the scanner reads
one function as a flat line, so a hazard in one closure can pair with a use in
another. Triage is by hand. This script's job is to keep the POPULATION from
growing silently, exactly like its sibling.

WHAT IT DELIBERATELY DOES NOT DO
--------------------------------

It does not decide reachability or exploitability, and it does not follow calls
(a helper that allocates two frames down is invisible unless its name is in
`HAZARD`). Both limits are why the baseline is a ratchet and not a zero.

Usage:
    scripts/stale-handle-across-alloc-audit.py            # report + compare
    scripts/stale-handle-across-alloc-audit.py --update   # re-baseline
    scripts/stale-handle-across-alloc-audit.py --detail   # every site
    scripts/stale-handle-across-alloc-audit.py --selftest # no tree needed

Exit: 0 ok · 1 the population GREW · 2 no baseline · 3 the gate is broken
"""
import argparse
import collections
import io
import os
import re
import sys

CRATES = ["native-builtins", "native-collections", "native-io", "native-api",
          "native-builtins-crypto", "native-builtins-security", "native-awt"]

# Calls that can allocate or run Java. `invoke*` counts because it dispatches
# interpreted bytecode, and `ensure_class_initialized` because it runs
# `<clinit>` — which is what moved the assertion in the BindableTests defect.
HAZARD = re.compile(
    r"\b(ensure_class_initialized\w*"
    r"|invoke(_virtual\w*|_special\w*|_static\w*|_by_class_id|_interface\w*)?\s*\("
    r"|new_object\w*|alloc_object\w*|try_alloc\w*"
    # gc-common w21: `new_ref_array_grow(` and friends are array allocators
    # too (w20-a).
    r"|new_array\w*|new_ref_array\w*|try_new_ref_array\w*|try_new_array\w*"
    r"|create_string\w*|create_string_from_units|create_ascii_case_string_cached"
    r"|get_class_mirror|primitive_class_mirror|load_class|define_class\w*"
    # gc-common w21: the open-ended prefixes count only as CALLS, so a plain
    # `alloc_id` / `box_count` variable is not a GC point (w20-e).
    r"|box_\w*\s*\(|native_map_init|native_list_init|intern_string"
    r"|alloc_\w*\s*\(|allocate_\w*\s*\("
    # ... and a helper whose name only CONTAINS `_alloc_` (`map_alloc_node`) is
    # an allocator as well (w20-a).
    r"|\w+_alloc_\w*\s*\("
    # gen r4w3/rooting: `monitor_wait` parks GC-blocked, so a peer may run a
    # moving collection meanwhile. (Plain `NativeContext::monitor_enter` is NOT
    # listed: it blocks as a counted mutator, which holds a STW off — see its
    # NOTE in vm_exec.rs; `monitor_enter_gc_safe` returns the moved ref.)
    r"|monitor_wait\w*)"
    # gc-common w19-f: a call rustfmt split after its `(` ends the line with
    # `(`, where `\b` cannot match; it is a GC point all the same.
    r"(?:\b|(?<=\())")

HAZARD_EXAMPLES = [
    "let c = ctx.ensure_class_initialized(\"java/lang/Object\");",
    "let v = ctx.invoke_virtual(o, \"m\", \"()V\", &[]);",
    "let o = ctx.new_object(\"java/lang/Object\");",
    "let o = ctx.alloc_object(cid, 2);",
    "let o = try_alloc_concurrent_synthetic(ctx, \"java/lang/Object\", 1)?;",
    "let a = ctx.new_array(ArrayElementType::Byte, 8);",
    "let a = ctx.new_ref_array(cid, 2);",
    "let a = ctx.try_new_ref_array(cid, 2);",
    "let s = ctx.create_string(\"x\");",
    "let s = ctx.create_string_from_units(&u);",
    "let m = ctx.get_class_mirror(cid);",
    "let m = ctx.primitive_class_mirror(cid);",
    "let c = ctx.load_class(\"java/lang/Object\");",
    "let c = ctx.define_class_from_bytes(&b);",
    "let b = crate::lang_class::box_value(ctx, v, d);",
    "cratonvm_native_collections::native_map_init(ctx, &args)?;",
    "let n = alloc_json_node(ctx, 1)?;",
    "let l = ctx.allocate_instance(cid);",
    "ctx.monitor_wait(this, 0);",
]

# A use in RECEIVER position: the first argument is the object being touched.
RECV_CALL = re.compile(
    r"\.(set_field_by_name|get_field_by_name|set_field|get_field"
    r"|invoke_virtual\w*|read_string|read_string_units"
    r"|array_length|get_array_element|set_array_element"
    r"|object_is_array|write_byte_array_from|read_byte_array_into"
    r"|set_array_element_checked)\s*\(\s*\*?([A-Za-z_]\w*)")
# (gc-common w21: `*p` -- a receiver read through a `&ObjectRef` / `&mut
# ObjectRef` -- is a use of `p` too; it used to be invisible, w20-e.)

BINDER = re.compile(
    r"\blet\s+(?:mut\s+)?([A-Za-z_]\w*)\s*(?::\s*[^=]*ObjectRef[^=]*)?=\s*(.*)$")
OBJ_RHS = re.compile(
    r"(alloc_object|new_object\w*|new_array|new_ref_array|try_new_\w*"
    r"|create_string\w*|read_native_pin|as_object\(\)|get_field_by_name"
    r"|ObjectRef\s*\{|\.object\(\)|\.get\(&"
    # gc-common w19 (common-w18b): helpers that hand back an object; without
    # them `let this = obj_arg(args, 0)?` binds at the body's header.
    r"|\bobj_arg\s*\(|\btry_alloc\w*\s*\("
    # gc-common w21: a global-root read hands back the object's current address.
    r"|\bresolve_global_root\s*\()")
READPIN = re.compile(r"read_native_pin\s*\(\s*([A-Za-z_]\w*)\s*,\s*([A-Za-z_]\w*)")
PINCALL = re.compile(r"(?:pin_native_root|handle_root|\.root)\s*\(\s*([A-Za-z_]\w*)")

# A name bound afresh: match arms, `if/while let`, closure parameters, `for`.
# Without these, a `Some(x) => x` arm reads as a use of an older `x`.
# gc-common w21: `Ok(x)` / `Err(x)` bind as `Some(x)` does, and an arm may
# carry a guard (`Some(x) if x.is_null() =>`) (w20-f).
REBIND = [
    re.compile(r"\b(?:Some|Ok|Err)\(\s*(?:mut\s+)?([A-Za-z_]\w*)\s*\)\s*\)*\s*(?:if\b.*?)?=>"),
    re.compile(r"\b(?:if|while)\s+let\s+.*?\b(?:Some|Ok|Err)\(\s*(?:mut\s+)?([A-Za-z_]\w*)\s*\)"),
    re.compile(r"\|\s*([A-Za-z_]\w*)\s*(?::[^|]*)?\|"),
    re.compile(r"\bfor\s+(?:mut\s+)?([A-Za-z_]\w*)\s+in\b"),
    re.compile(r"\blet\s+(?:Some|Ok|Err)\(\s*(?:mut\s+)?([A-Za-z_]\w*)\s*\)"),
]
# A refresh: `let v = scope.get(&v_h);` / `v = ctx.read_native_pin(h, v);`
# Id counters whose names carry `alloc`, but which never touch the heap
# (socket / listener / zone / engine ids): not GC points (gc-common w21).
NOT_GC = re.compile(r"\b(?:s2_alloc_\w*|alloc_\w*_id|\w+_alloc_id|allocate_\w*_id"
                    r"|allocate_native_memory|box_cif_\w*)\s*\(")
# `let Ok(Some(Value::Object(Some(x)))) =` (a pattern `let`, often a let-else
# whose call is on the next line) binds `x` (gc-common w21, common-w21d).
PATTERN_LET = re.compile(r"^\s*let\s+[A-Z][\w:]*\(.*?\bSome\(\s*(?:mut\s+)?([A-Za-z_]\w*)\s*\)")
TUPLE_LET = re.compile(r"\blet\s+\(([^)=]*)\)\s*=")
# ... and so does `let [a, b] = live;` (gc-common w21, w21-a).
ARRAY_LET = re.compile(r"\blet\s+\[([^\]=]*)\]\s*=")
ROOTED_ACROSS = re.compile(r"\brooted_across\s*\(\s*\w+\s*,\s*&mut\s*\[([^\]]*)\]")
REFRESH_LET = re.compile(r"\blet\s+(?:mut\s+)?([A-Za-z_]\w*)\s*=\s*[a-z_]*\.get\(&")
REFRESH_ASSIGN = re.compile(
    r"^\s*\*?([A-Za-z_]\w*)\s*=\s*.*(?:read_native_pin|resolve_global_root)")
# (`*p = ctx.read_native_pin(pin, *p)` refreshes `p` too: common-w21d.)

# A line on which control leaves the body: a GC point there cannot precede a
# later use on the same path (gc-common w19, common-w18b proposed fix 3).
RETURNS = re.compile(r"^\s*(?:return\b|[^/]*=>\s*return\b)")

# A string literal, or a char literal: `'{'` / `'}'` counted as braces used to
# run a function to the end of its FILE (common-w20b). A lifetime (`'a`) has no
# closing quote after one character, so it is left alone.
STR_LIT = re.compile(r'"(?:[^"\\]|\\.)*"'
                     r"|'(?:[^'\\\n]|\\(?:u\{[0-9A-Fa-f]+\}|x[0-9A-Fa-f]{2}|.))'")
FNDEF = re.compile(r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?"
                   r"(?:extern\s+\"[^\"]*\"\s+)?fn\s+([A-Za-z_]\w*)")
CFG_TEST = re.compile(r"\s*#\[cfg\(test\)\]")


def strip_code(line):
    """Drop string literals and line comments — enough for a token screen."""
    return STR_LIT.sub('""', line).split("//")[0]


def functions(lines):
    """(name, first_line_index, body_lines) per `fn`, by brace balance."""
    out, i = [], 0
    while i < len(lines):
        m = FNDEF.match(lines[i])
        if not m:
            i += 1
            continue
        depth, started, j = 0, False, i
        while j < len(lines):
            c = strip_code(lines[j])
            depth += c.count("{") - c.count("}")
            if "{" in c:
                started = True
            if started and depth <= 0:
                break
            j += 1
        out += split_closures(m.group(2), i, lines[i:j + 1])
        i = j + 1
    return out


CLOSURE = re.compile(r"\|\s*[A-Za-z_&\s,:<>'\w]*\|\s*(->[^{]*)?\{\s*$")


def split_closures(name, start, body):
    """Split a body into the outer body and each multi-line closure body.

    A `register_*` function is a hundred `r.register(cls, m, sig, |ctx, args| {
    … })` blocks that share nothing but the registry. Scanning them as one flat
    line pairs an allocation in one closure with a use in another and reports a
    defect that cannot happen — two of the three survivors of the 2026-09-11
    triage were exactly that. Each closure is its own body, and the lines it
    occupies are blanked out of the parent so nothing is scanned twice.
    """
    kept = list(body)
    out = []
    k = 0
    nth = 0
    while k < len(kept):
        c = strip_code(kept[k])
        if not CLOSURE.search(c):
            k += 1
            continue
        depth, j = 0, k
        while j < len(kept):
            cc = strip_code(kept[j])
            depth += cc.count("{") - cc.count("}")
            if depth <= 0 and j > k:
                break
            j += 1
        if j - k >= 3:
            # Keyed by ORDINAL, not by line: a baseline keyed on a line number
            # renames every closure in a file the moment anything above it
            # grows a line, and the gate then reports a tree that did not
            # change as a tree that grew.
            nth += 1
            out.append(("%s{closure#%d}" % (name, nth), start + k,
                        kept[k:j + 1]))
            # The opening line keeps what precedes the closure (gc-common w21,
            # common-w21d): `let x = rooted_across(ctx, &mut [..], |ctx| {`
            # still binds `x` and refreshes the slice's names in the parent.
            opener = CLOSURE.search(c)
            head = c[:opener.start()] if opener else ""
            for z in range(k, min(j + 1, len(kept))):
                kept[z] = ""
            kept[k] = head
        k = j + 1
    out.append((name, start, kept))
    return out


def statement_ends(code):
    """For each line, the last line of the statement it starts.

    A multi-line `let x = match ctx.invoke_virtual(...) { ... };` binds and
    allocates in ONE statement; without this the scanner reports the binding
    against its own initialiser.
    """
    ends = {}
    for k in range(len(code)):
        depth, j = 0, k
        while j < len(code) and j < k + 24:
            depth += code[j].count("(") - code[j].count(")")
            depth += code[j].count("{") - code[j].count("}")
            t = code[j].rstrip()
            if depth <= 0 and (t.endswith(";") or t.endswith("{") or t.endswith("}")):
                break
            j += 1
        ends[k] = j
    return ends


def cfg_test_lines(lines):
    """1-based numbers of the lines inside an item marked `#[cfg(test)]`.

    gc-common w19 (common-w19c): the gate used to treat everything below a
    file's FIRST `#[cfg(test)]` as test code, which hid every production
    function after a test-only helper or a mid-file test module. The region is
    now the item each attribute applies to: further attribute lines are
    skipped, then the item runs to its balancing `}` (or its `;` when it has
    no body).
    """
    out = set()
    i = 0
    while i < len(lines):
        if not CFG_TEST.match(lines[i]):
            i += 1
            continue
        j = i + 1
        while j < len(lines) and strip_code(lines[j]).strip().startswith("#["):
            j += 1
        depth, started, k = 0, False, j
        while k < len(lines):
            c = strip_code(lines[k])
            depth += c.count("{") - c.count("}")
            if "{" in c:
                started = True
            if started and depth <= 0:
                break
            if not started and c.rstrip().endswith(";"):
                break
            k += 1
        out.update(range(i + 1, k + 2))
        i = k + 1
    return out


def scan_lines(lines, path="<mem>"):
    hits = []
    test_lines = cfg_test_lines(lines)
    for name, start, body in functions(lines):
        code = [strip_code(l) for l in body]
        objvars = {}
        lets = collections.defaultdict(list)
        for k, l in enumerate(code):
            m = BINDER.search(l)
            if not m:
                continue
            lets[m.group(1)].append(k)
            rhs = m.group(2)
            if not rhs.strip():
                # gc-common w21: `let x =` with the call on the next line (w19-b).
                rhs = next((c for c in code[k + 1:k + 3] if c.strip()), "")
            if OBJ_RHS.search(rhs) or "ObjectRef" in l:
                # The FIRST object binding; a later `let` of the same name is a
                # fresh binding (a rebind below), not the one to track -- it
                # used to replace it, which hid every site of the earlier one.
                objvars.setdefault(m.group(1), k)
        # The body's opening line: the header ends at the first line whose code
        # ends in `{`. An argument-derived variable is live from there.
        # (gc-common w19, common-w18b: it used to be bound at line 0, whose
        # "statement" ran to the 24-line cap, so no GC point in the first 24
        # lines of any body was ever considered for it.)
        opening = next((k for k, l in enumerate(code) if l.rstrip().endswith("{")), 0)
        argvars = set()
        for l in code:
            for m in RECV_CALL.finditer(l):
                v = m.group(2)
                if v not in objvars and v not in ("self", "ctx", "scope"):
                    objvars[v] = opening    # argument-derived: live from entry
                    argvars.add(v)
        ends = statement_ends(code)
        rebinds = collections.defaultdict(set)
        for v, ks in lets.items():
            # A `let` binds once its whole statement has run (a short closure
            # argument is not split out and stays in these lines).
            rebinds[v].update(ks)
            rebinds[v].update(ends.get(k, k) for k in ks)
        for k, l in enumerate(code):
            for rx in REBIND:
                for m in rx.finditer(l):
                    rebinds[m.group(1)].add(k)
            m = PATTERN_LET.match(l)
            if m:
                # Bound once its initialiser has run: at the statement's end.
                rebinds[m.group(1)].update((k, ends.get(k, k)))
            # gc-common w21 (w20-a): `let (this, out) = rooted_across1(..)`
            # binds every name in the tuple, and `rooted_across(ctx, &mut
            # [&mut a, &mut b], ..)` hands `a` and `b` back current.
            for rx in (TUPLE_LET, ARRAY_LET, ROOTED_ACROSS):
                for m in rx.finditer(l):
                    for n in re.findall(r"(?:&\s*)?(?:mut\s+)?([A-Za-z_]\w*)", m.group(1)):
                        rebinds[n].update((k, ends.get(k, k)))
        for k, l in enumerate(code):
            for rx in (REFRESH_LET, REFRESH_ASSIGN):
                m = rx.search(l)
                if m:
                    # A refresh restarts the variable, but an earlier binding
                    # stays tracked: a stale use BEFORE the refresh is still a
                    # site (it used to move the binding forward and hide it).
                    objvars.setdefault(m.group(1), k)
                    rebinds[m.group(1)].add(k)
        pinned = {m.group(1) for l in code for m in PINCALL.finditer(l)}

        for v, bind in sorted(objvars.items()):
            if v in ("ctx", "self", "scope"):
                continue
            haz = None
            for k in range(bind + 1, len(code)):
                l = code[k]
                if k in rebinds[v]:
                    haz = None              # a fresh binding: start over
                    continue
                if haz is None:
                    if v not in argvars and k <= ends.get(bind, bind):
                        continue            # still inside v's own statement
                    if RETURNS.match(l):
                        continue            # control leaves the body here
                    if HAZARD.search(NOT_GC.sub("", l)) and not re.search(
                            r"\blet\s+(?:mut\s+)?" + re.escape(v) + r"\b", l):
                        haz = k
                    continue
                if v not in [m.group(2) for m in RECV_CALL.finditer(l)]:
                    continue
                rp = READPIN.search(l)
                if rp and rp.group(2) == v:
                    continue                # the fallback operand, not a use
                if re.search(r"\blet\s+(?:mut\s+)?" + re.escape(v) + r"\b", l):
                    continue
                hits.append(dict(file=path, fn=name, var=v, pinned=v in pinned,
                                 bind=start + bind + 1, hazard=start + haz + 1,
                                 use=start + k + 1,
                                 hazard_src=code[haz].strip()[:110],
                                 use_src=l.strip()[:110],
                                 in_test=(start + k + 1) in test_lines))
                break
    return hits


TEST_MOD = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*;")
PATH_ATTR = re.compile(r'^\s*#\[path\s*=\s*"([^"]+)"\s*\]')


def test_only_files(path, lines):
    """Files (and module directories) that `path` declares `#[cfg(test)] mod x;`.

    gc-common w21 (w20-f): a file included only by a test-gated `mod x;` has
    no `#[cfg(test)]` of its own, so it was scanned as production code.
    """
    d = os.path.dirname(path)
    stem = os.path.splitext(os.path.basename(path))[0]
    # `foo.rs` declares its children in `foo/`; `lib.rs`, `main.rs`, `mod.rs`
    # declare them beside themselves.
    child_dir = d if stem in ("lib", "main", "mod") else os.path.join(d, stem)
    out = []
    for i, l in enumerate(lines):
        if not CFG_TEST.match(l):
            continue
        explicit = None
        j = i + 1
        while j < len(lines) and lines[j].strip().startswith("#["):
            pm = PATH_ATTR.match(lines[j])
            if pm:
                explicit = os.path.normpath(os.path.join(d, pm.group(1)))
            j += 1
        m = TEST_MOD.match(lines[j]) if j < len(lines) else None
        if not m:
            continue
        if explicit:
            out.append(explicit)
        else:
            out.append(os.path.join(child_dir, m.group(1) + ".rs"))
            out.append(os.path.join(child_dir, m.group(1)))   # its mod.rs / children
    return [os.path.normcase(os.path.normpath(p)) for p in out]


def scan_tree(root):
    files = []
    for crate in CRATES:
        base = os.path.join(root, crate, "src")
        for dirpath, _, names in os.walk(base):
            for f in sorted(names):
                if f.endswith(".rs"):
                    p = os.path.join(dirpath, f)
                    files.append((p, io.open(p, encoding="utf-8",
                                             errors="replace").read().splitlines()))
    excluded = set()
    for p, lines in files:
        excluded.update(test_only_files(p, lines))

    def is_test_file(p):
        q = os.path.normcase(os.path.normpath(p))
        return q in excluded or any(q.startswith(e + os.sep) for e in excluded)

    hits = []
    for p, lines in files:
        if is_test_file(p):
            continue
        rel = os.path.relpath(p, root).replace("\\", "/")
        hits += [h for h in scan_lines(lines, rel) if not h["in_test"]]
    return hits


def selftest():
    bad = 0
    for ex in HAZARD_EXAMPLES:
        if not HAZARD.search(strip_code(ex)):
            print("  DEAD HAZARD TOKEN, no longer matches: %s" % ex)
            bad += 1
    positive = """
fn build(ctx: &mut dyn NativeContext) -> ObjectRef {
    let arr = ctx.new_ref_array(cid, 2);
    let e = ctx.alloc_object(cid, 1);
    ctx.set_array_element(arr, 0, Value::Object(Some(e)));
    arr
}
""".splitlines()
    hits = scan_lines(positive)
    if len(hits) != 1 or hits[0]["var"] != "arr":
        print("  the detector missed the canonical positive: %r" % (hits,))
        bad += 1
    negative = """
fn build(ctx: &mut dyn NativeContext) -> ObjectRef {
    let mut scope = NativeHandleScope::new(ctx);
    let arr_obj = scope.new_ref_array(cid, 2);
    let arr_h = scope.root(arr_obj);
    let e = scope.alloc_object(cid, 1);
    let arr = scope.get(&arr_h);
    scope.set_array_element(arr, 0, Value::Object(Some(e)));
    arr
}
""".splitlines()
    hits = scan_lines(negative)
    if hits:
        print("  the detector flagged the FIXED form: %r" % (hits,))
        bad += 1
    # gc-common w21: a `'{'` char literal must not merge two functions, an
    # `alloc_id` variable is not a GC point, and an `Ok(x) =>` arm rebinds `x`.
    shapes = """
fn braces(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    if c == '{' { depth += 1; }
    ctx.set_field(obj, 0, Value::Int(alloc_id));
}
fn after(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    let s = ctx.create_string("x");
    match ctx.invoke_virtual(s, "m", "()V", &[]) {
        Ok(obj) => ctx.set_field(obj, 0, Value::Int(1)),
        Err(e) => return Err(e),
    }
}
""".splitlines()
    hits = scan_lines(shapes)
    if hits:
        print("  a char literal, a plain alloc_ name or an Ok arm misread: %r" % (hits,))
        bad += 1
    print("  selftest: %s" % ("BROKEN" if bad else "ok"))
    return 3 if bad else 0


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=os.path.dirname(here))
    ap.add_argument("--baseline",
                    default=os.path.join(here, "baselines",
                                         "stale-handle-across-alloc-sites.txt"))
    ap.add_argument("--detail", action="store_true")
    ap.add_argument("--update", action="store_true")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()

    if a.selftest:
        return selftest()
    rc = selftest()
    if rc:
        return rc

    hits = scan_tree(a.root)
    per_fn = collections.Counter("%s::%s" % (h["file"], h["fn"]) for h in hits)
    print("  %d site(s) across %d function(s)" % (len(hits), len(per_fn)))
    if a.detail:
        for h in sorted(hits, key=lambda h: (h["file"], h["use"])):
            print("  %s:%d %s::%s  (bound %d, allocates %d, pinned=%s)"
                  % (h["file"], h["use"], h["fn"], h["var"], h["bind"],
                     h["hazard"], h["pinned"]))
            print("      alloc: %s" % h["hazard_src"])
            print("      use:   %s" % h["use_src"])

    if a.update:
        os.makedirs(os.path.dirname(a.baseline), exist_ok=True)
        with io.open(a.baseline, "w", encoding="utf-8", newline="\n") as fh:
            fh.write("# stale-handle-across-allocation baseline"
                     " — scripts/stale-handle-across-alloc-audit.py\n")
            fh.write("# <count>\t<file>::<fn> — a RATCHET, not a target:"
                     " triage is by hand and a match is not a defect.\n")
            for key in sorted(per_fn):
                fh.write("%d\t%s\n" % (per_fn[key], key))
        print("  baseline written: %d fn(s), %d site(s)" % (len(per_fn), len(hits)))
        return 0

    if not os.path.exists(a.baseline):
        print("  NO BASELINE at %s — run --update" % a.baseline)
        return 2
    base = {}
    for line in io.open(a.baseline, encoding="utf-8"):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        n, key = line.split("\t", 1)
        base[key] = int(n)
    grew = 0
    for key in sorted(per_fn):
        if per_fn[key] > base.get(key, 0):
            print("  TRIPPED: %s now has %d site(s) (baseline %d)"
                  % (key, per_fn[key], base.get(key, 0)))
            grew += 1
    if grew:
        print("  %d function(s) grew. Root the reference in a NativeHandleScope"
              " and read it back after the allocation, or re-baseline"
              " deliberately with --update." % grew)
        return 1
    shrank = sum(1 for key in base if per_fn.get(key, 0) < base[key])
    print("  ok — nothing grew%s" % (", %d shrank" % shrank if shrank else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
