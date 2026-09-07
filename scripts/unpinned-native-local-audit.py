#!/usr/bin/env python3
"""Unpinned-native-local audit for one crate.

THE SHAPE (the `unpinned-native-locals-audit-48-fixed-20260825` family):

    a native holds an `ObjectRef` / `Value::Object` across a call that can
    trigger a GC, and uses it afterwards.

Under a moving collector the local goes stale. Under the Generational
collector's NON-MOVING young sweep an object nothing else roots is ZEROED in
place — which is what `native_fcimpl_open` did to a live FileChannelImpl.

TWO RULES, because they see different things:

  rule 1 (local)     `let X = <GC-capable call>` … <GC-capable call> … use X
  rule 2 (parameter) a parameter `X: ObjectRef` used after a GC-capable call.
                     Rule 1 structurally cannot see this: a parameter is never
                     `let`-bound. That blind spot is why the 2026-08-25 audit
                     needed two rules, and rule 2 found 40 of its 48.

STATEMENTS, NOT LINES. The first version of this scanned lines and reported
correctly-converted code, because

    let channel = scope.get(&channel_h);
    let x = scope.new_object_initialized(
        "…",
        &[Value::Object(Some(channel))],     ← "use" on a LATER LINE
    );

passes `channel` as an ARGUMENT of the allocating call — evaluated before the
call runs, so it is the current address and perfectly safe. Reassembling
statements (paren/brace depth, split on `;` at depth 0) makes "used after the
call returns" the thing being asked, which is the actual defect.

WHY THE LEVEL-0 SET IS RESTATED HERE rather than imported from
`scripts/stale-receiver-audit.py`: that gate's `ALLOC0` carries three tokens
matching NOTHING in the tree (`ctx.new_string`, `ctx.intern`, `ctx.box_` — 0
occurrences each) and omits the most common allocator in these crates,
`ctx.create_string` (2483 uses), plus `new_object`, `new_object_initialized`,
`new_ref_array` and `get_class_mirror`. It also omits
`begin_blocking_region`/`end_blocking_region`, which for an I/O crate is the
important one: a PEER thread's collection runs while this thread is blocked, and
the existence of `end_blocking_region_refs` — which does rewrite native locals —
is the proof that the plain form does not.

`set_field_by_name` is deliberately NOT GC-capable: it takes `&self`, resolves a
field index out of the class store and stores. It neither allocates nor runs
Java.

NOT DONE HERE, deliberately: deciding whether a site is exploitable. That needs
a reproduction. This ranks nothing; it produces a population to READ.
"""
import argparse
import collections
import glob
import io
import os
import re

ALLOC0 = re.compile(
    r"\.(?:"
    r"new_object|new_object_initialized|new_object_initialized_with_class_id"
    r"|new_array|new_ref_array|try_new_array|try_new_ref_array"
    r"|alloc_object|allocate_instance|try_alloc_object_gc_safe|fresh_object"
    r"|create_string|create_string_from_units|create_string_uninterned"
    r"|create_string_uninterned_gc_safe|init_string_from_units"
    r"|create_ascii_case_string_cached|get_ascii_case_string_cached"
    r"|get_class_mirror|primitive_class_mirror|cache_module_mirror"
    r"|invoke|invoke_virtual|invoke_special|invoke_by_class_id"
    r"|invoke_special_by_class_id|invoke_virtual_bytecode_only"
    r"|invoke_special_bytecode_only|invoke_virtual_declared"
    r"|ensure_class_initialized|ensure_class_initialized_with_class_id"
    r"|initialize_class|load_class|define_class_from_bytes|define_class_full"
    r"|define_class_with_loader|define_hidden_class_from_bytes"
    r"|force_gc|reclaim_before_alloc_retry"
    r"|begin_blocking_region|begin_timed_blocking_region|end_blocking_region"
    r"|monitor_enter_gc_safe|monitor_wait|park|thread_join"
    r"|capture_stack_trace|capture_throwable_stack_trace|get_stack_trace"
    r"|declared_fields|declared_methods|class_annotations|record_components"
    r")\s*\("
    r"|\balloc_ref_array\b|\btry_alloc_synthetic\b"
)

# Rooting. `scope.get` counts: a handle re-read is the SAFE spelling, and a rule
# that does not know that reports every correctly-converted site as a defect.
ROOT = re.compile(
    r"\b(?:pin_native_root|read_native_pin|unpin_native_roots"
    r"|handle_root|handle_get|NativeHandleScope|add_global_root"
    r"|resolve_global_root|end_blocking_region_refs)\b"
    r"|\bscope\s*\.\s*(?:root|get)\b"
)

# Leading whitespace ALLOWED. Anchoring at column 0 left every function
# inside `mod tests` or an `impl` block unindexed, and glued its body onto
# the previous top-level fn — which is how 23 `#[test]` bodies in
# lang_string.rs were reported under `register_phase52_string_buffer`.
FNDEF = re.compile(r"^\s*(?:pub(?:\([a-z ]+\))? )?(?:async )?(?:unsafe )?fn ([a-z_][a-z_0-9]*)")
LET = re.compile(r"^\s*let\s+(?:mut\s+)?([a-z_][a-z_0-9]*)\s*[:=]")
CALLEE = re.compile(r"(?<![a-z_0-9.])([a-z_][a-z_0-9]*)\s*\(")
PARAM_REF = re.compile(r"\b([a-z_][a-z_0-9]*)\s*:\s*(?:&mut\s+)?ObjectRef\b")

# A binding can only go stale if it HOLDS A REFERENCE. `epoch` binds
# `let month = invoke_i32(ctx, obj, "getMonthValue")` — GC-capable RHS, used
# later, and an `i32`: not this defect, and six such bindings in one function
# were the largest remaining false-positive source.
#
# Two ways to qualify. (a) the RHS is a known ObjectRef-returning allocator, or
# (b) the name is used SOMEWHERE in the function in a position that only a
# reference can occupy. (b) is what keeps `get_field`-derived references — the
# other half of the family — from being filtered out with the integers.
REF_RHS = re.compile(
    r"\.(?:new_object|new_object_initialized|new_object_initialized_with_class_id"
    r"|new_array|new_ref_array|try_new_array|try_new_ref_array"
    r"|alloc_object|allocate_instance|try_alloc_object_gc_safe|fresh_object"
    r"|create_string|create_string_from_units|create_string_uninterned"
    r"|create_string_uninterned_gc_safe|init_string_from_units"
    r"|create_ascii_case_string_cached|get_ascii_case_string_cached"
    r"|get_class_mirror|primitive_class_mirror)\s*\("
    r"|\bnew_object_ref\s*\(|\balloc_ref_array\s*\(|\btry_alloc_synthetic\s*\("
    # A `Value` can BE a reference. `let cleaner = ctx.invoke(..).ok().flatten()`
    # is an `Option<Value>` destructured later as `Value::Object(Some(x))` under
    # a DIFFERENT name, so no use-site test can see it — and it was one of the
    # seven real bindings in the positive control, dropped silently until this
    # was added back.
    r"|\.(?:invoke|invoke_virtual|invoke_special|invoke_by_class_id"
    r"|invoke_special_by_class_id|invoke_virtual_bytecode_only"
    r"|invoke_special_bytecode_only|invoke_virtual_declared"
    r"|get_field|get_field_by_name|get_field_typed|get_field_volatile"
    r"|get_static_field|get_array_element)\s*\("
)


def ref_use(name, text):
    n = re.escape(name)
    return re.search(
        r"Value::Object\s*\(\s*Some\s*\(\s*" + n + r"\s*\)"
        r"|\.(?:get_field|set_field|get_field_by_name|set_field_by_name"
        r"|get_field_typed|get_field_volatile|set_field_volatile"
        r"|invoke_virtual|invoke_special|invoke_virtual_declared"
        r"|class_id_of_object|array_length|get_array_element|set_array_element"
        r"|identity_hash_code|monitor_enter|read_string|object_is_array"
        r"|heap_kind_of|object_num_fields)\s*\(\s*" + n + r"\b"
        r"|(?:scope\s*\.\s*root|pin_native_root|handle_root|add_global_root)"
        r"\s*\(\s*" + n + r"\b"
        r"|\b" + n + r"\s*\.\s*as_ptr\s*\(",
        text,
    ) is not None


def REBIND(name):
    n = re.escape(name)
    return re.compile(
        r"\b(?:if|while)\s+let\s+[^=]*\b" + n + r"\b[^=]*="
        r"|\bfor\s+(?:\(\s*)?" + n + r"\b[^;]*\bin\b"
        r"|\|[^|]*\b" + n + r"\b[^|]*\|"
    )


Fn = collections.namedtuple("Fn", "name file line body is_test")
Stmt = collections.namedtuple("Stmt", "line text")


def strip_comments(lines):
    """CODE ONLY — the existing gate learned this the hard way: three of its
    first 21 candidates were `ctx.alloc_object` in a DOC COMMENT."""
    out, in_block = [], False
    for l in lines:
        s = l.strip()
        if in_block:
            if "*/" in s:
                in_block = False
            out.append("")
            continue
        if s.startswith("/*"):
            in_block = "*/" not in s
            out.append("")
            continue
        if s.startswith("//"):
            out.append("")
            continue
        out.append(l)
    return out


def test_spans(lines):
    """Line ranges under `#[cfg(test)]` / `mod tests` / `#[test]`.

    Test bodies drive a MOCK context with no moving GC (`mock_ctx()`), so a
    stale local there is not a defect. Left in, they were the single largest
    false-positive source."""
    spans, i, n = [], 0, len(lines)
    while i < n:
        s = lines[i].strip()
        if s.startswith("#[cfg(test)]") or s.startswith("#[test]") or \
           re.match(r"^(pub )?mod tests\b", s):
            depth, j, seen = 0, i, False
            while j < n:
                depth += lines[j].count("{") - lines[j].count("}")
                if "{" in lines[j]:
                    seen = True
                if seen and depth <= 0:
                    break
                j += 1
            spans.append((i, j))
            i = j + 1
            continue
        i += 1
    return spans


def index(paths):
    fns = []
    for f in paths:
        raw = io.open(f, encoding="utf-8", errors="replace", newline="").read().split("\n")
        lines = strip_comments(raw)
        tspans = test_spans(lines)
        idx = [i for i, l in enumerate(lines) if FNDEF.match(l)]
        for n, i in enumerate(idx):
            j = idx[n + 1] if n + 1 < len(idx) else len(lines)
            is_test = any(a <= i <= b for (a, b) in tspans)
            fns.append(Fn(FNDEF.match(lines[i]).group(1), f, i + 1, lines[i:j], is_test))
    return fns


def statements(body):
    """Reassemble the body into statements: split on `;` at paren/brace depth 0.

    This is what makes "used after the call RETURNS" askable — a use inside the
    same statement is an argument, evaluated before the call runs."""
    out, buf, start, depth = [], [], 0, 0
    for i, l in enumerate(body):
        if not buf:
            start = i
        buf.append(l.strip())
        # Parens and brackets only. A `{` opens a BLOCK, not a continuation —
        # counting it made the `fn ...(...) {` signature line leave the depth
        # permanently positive, no statement ever closed, and the whole audit
        # reported zero. A rule that dismisses everything is the one failure
        # mode the 2026-08-25 write-up singles out as worse than noise.
        depth += l.count("(") + l.count("[")
        depth -= l.count(")") + l.count("]")
        # Close on `;` always. Close on a bare `{`/`}` ONLY when the buffer is
        # not a `let` — otherwise `let pat = match .. { Ok(c) => .., };` is torn
        # into three "statements", the arm containing `alloc_object` reads as an
        # intervening GC-capable call, and the store on the next line reads as a
        # stale use. That shape alone produced several false positives.
        txt = l.rstrip()
        starts_let = buf and buf[0].startswith("let ")
        if depth <= 0 and (txt.endswith(";")
                           or (not starts_let and (txt.endswith("{") or txt.endswith("}")))):
            out.append(Stmt(start, " ".join(buf)))
            buf, depth = [], 0
    if buf:
        out.append(Stmt(start, " ".join(buf)))
    return out


def allocating(fns, depth):
    alloc = {fn.name: 0 for fn in fns if ALLOC0.search("\n".join(fn.body))}
    callees = [(fn.name, set(CALLEE.findall("\n".join(fn.body)))) for fn in fns]
    for d in range(1, depth + 1):
        known = set(alloc)
        add = {n: d for (n, cs) in callees if n not in alloc and not cs.isdisjoint(known)}
        if not add:
            break
        alloc.update(add)
    return alloc


def gc_capable(text, allocfns):
    if ALLOC0.search(text):
        return True
    for c in CALLEE.findall(text):
        if c in allocfns and c not in ("if", "while", "match", "for", "return", "Some", "Ok"):
            return True
    return False


def scan(fn, allocfns, want_params):
    stmts = statements(fn.body)
    hits = []

    if want_params:
        sig = "\n".join(fn.body[:12]).split("{")[0]
        for p in PARAM_REF.findall(sig):
            gc_at = None
            for k, st in enumerate(stmts):
                if ROOT.search(st.text):
                    break
                if gc_at is None:
                    if gc_capable(st.text, allocfns):
                        gc_at = k
                    continue
                if re.search(r"\b" + re.escape(p) + r"\b", st.text):
                    if st.text.strip().startswith("return") or "=>" in st.text:
                        continue
                    hits.append((fn.line + stmts[gc_at].line, p,
                                 fn.line + st.line, "param"))
                    break

    for i, st in enumerate(stmts):
        m = LET.match(st.text)
        if not m:
            continue
        name = m.group(1)
        if name == "_":
            continue
        if not gc_capable(st.text, allocfns):
            continue
        # A binding whose RHS ROOTS something is a HANDLE (an opaque slot), not
        # an `ObjectRef`. Handles are exactly what cannot go stale — that is the
        # point of the handle scope — so a later use of one is not this defect.
        if re.search(r"\b(?:scope\s*\.\s*root|handle_root|pin_native_root)\s*\(", st.text):
            continue
        if not REF_RHS.search(st.text) and not ref_use(name, "\n".join(fn.body)):
            continue
        gc_at = None
        rooted_elsewhere = False
        for k in range(i + 1, len(stmts)):
            t = stmts[k].text
            if ROOT.search(t):
                # Break ONLY when the rooting names THIS binding. Breaking
                # on any rooting at all is a confident dismissal: in the
                # pre-fix `native_fcimpl_open` a `pin_native_root(channel)`
                # two thirds of the way down hid `position_lock`,
                # `dispatcher` and `threads`, every one genuinely unrooted.
                # The 2026-08-25 write-up's single FALSE NEGATIVE was of
                # exactly this kind, and it calls that "the dangerous kind".
                if re.search(r"\b" + re.escape(name) + r"\b", t):
                    break
                rooted_elsewhere = True
            # A re-binding is a FRESH value, not a stale use — and `let` is
            # not the only way to make one. `if let Some(obj) = ...` in a later
            # branch shadowed the outer `obj` in `bb_derive_view` and was
            # reported as a use of it.
            m2 = LET.match(t)
            if m2 and m2.group(1) == name:
                break
            if REBIND(name).search(t):
                break
            if gc_at is None:
                if gc_capable(t, allocfns):
                    gc_at = k
                continue
            if re.search(r"\b" + re.escape(name) + r"\b", t):
                kind = "local*" if rooted_elsewhere else "local"
                hits.append((fn.line + st.line, name, fn.line + stmts[k].line, kind))
                break
    return hits


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("glob")
    ap.add_argument("--depth", type=int, default=6)
    ap.add_argument("--detail", action="store_true")
    ap.add_argument("--tests", action="store_true", help="include test bodies")
    ap.add_argument("--only", default=None)
    a = ap.parse_args()
    fns = index(sorted(glob.glob(a.glob)))
    allocfns = allocating(fns, a.depth)
    rows, skipped = [], 0
    for fn in fns:
        if fn.is_test and not a.tests:
            skipped += 1
            continue
        if a.only and fn.name != a.only:
            continue
        for (ln, nm, use, kind) in scan(fn, allocfns, True):
            rows.append((os.path.basename(fn.file), ln, fn.name, nm, use, kind))
    per = collections.Counter(r[0] for r in rows)
    print("functions indexed: %d (test bodies skipped: %d) ; reachable-allocating: %d"
          % (len(fns), skipped, len(allocfns)))
    for f, n in per.most_common():
        print("  %-28s %d" % (f, n))
    print("TOTAL candidates: %d" % len(rows))
    if a.detail:
        for r in sorted(rows):
            print("  %-24s:%-6d %-40s %-20s use@%-6d %s" % r)


main()
