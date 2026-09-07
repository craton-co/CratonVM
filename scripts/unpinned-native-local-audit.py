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
                     needed two rules.

NOT the same rule as that audit's rule 2, and an earlier version of this file
said it was. THAT rule was "two or more `ctx.invoke_*` on the same receiver in
one scope with no `read_native_pin` between", which is how it found 40 of the
48 — it keys on repeated invokes, and most of what it caught was `let`-bound
out of `args`, not declared in a signature. This one keys on the DECLARED type,
so the two populations overlap without either containing the other.

WHAT `--opt` SEES, CORRECTED 2026-09-07. `args: &[Value]` — the shape of every
native entry point, and the one `safe_native_call_impl` rebuilds only for
collections it runs ITSELF, before the callback. A native that re-enters Java
keeps naming the pre-call address. `Option<ObjectRef>`, `&[ObjectRef]` and
`Vec<ObjectRef>` are in the same tranche.

This paragraph used to say the rule "cannot see" those, and that was true for
the wrong reason: `PARAM_OPT` listed all five shapes and FOUR OF THEM MATCHED
NOTHING, because a trailing `` after `]`/`>` demands a word character and a
parameter list supplies `,` or `)`. `--opt` scanned only the bare `Value` arm
while advertising the rest. See the note on `PARAM_OPT` itself; the three print
natives that crashed under Generational are the positive control, and this
script reported the same count over that file before and after their fix, with
`--opt` or without.

A SLICE IS NOT A REFERENCE, which is why `slice_ref_use` gates this tranche.
"The statement mentions `args`" is the function's own argument list, not a
defect: only a `Value::Object` destructured out of the slice, or the slice
handed whole to a callee that will do it, can be stale. Unfiltered the tranche
is 598 candidates across the native crates; gated it is 500, and it is
depth-INSENSITIVE (53 vs 55 in `native-io` at `--depth 1` and `6`), so it is not
an artifact of transitive reachability.

WHAT A PARAMETER HIT MEANS, which is not what a local hit means.
`safe_native_call_impl` pins every argument of a native call into
`thread.native_pin_roots`, so an object that arrived through a native entry is
NOT reclaimable underneath its holder — the "zeroed in place" half of the family
does not apply to it. What does apply is RELOCATION: the pin keeps the object
alive and the snapshot keeps the old address, and Generational young relocates
by Cheney copy on the moving path and by selective promotion even on the
non-moving one. A parameter whose object was allocated by the CALLER rather than
passed in from Java has neither protection.

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
    # `get_ascii_case_string_cached` is NOT here, and its sibling is: the
    # getter (`vm_exec.rs`) walks `thread.string_case_cache` and returns a
    # `ObjectRef` it finds — no allocation, no Java. Listing it made every
    # statement that merely CONSULTS the cache read as a collection point, and
    # `string_case_impl` was reported on exactly that: a cache hit RETURNS, and
    # a miss allocates nothing. It stays in `REF_RHS`, where "binds a
    # reference" is the right claim.
    r"|create_ascii_case_string_cached"
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
    # `capture_stack_trace` / `capture_throwable_stack_trace` / `get_stack_trace`
    # are NOT here. `vm_exec::capture_current_stack_trace` takes `&self`, reads
    # the class store and this thread's frames, and returns a Rust
    # `Vec<StackTraceEntry>`; no Java object is allocated and no bytecode runs.
    # Listing them made three sites read as defects on the strength of a
    # diagnostic capture — `native_fetch_stack_frames`,
    # `delegate_to_real_bytecode` and `native_classloader_define_class1`, the
    # last of which only captures at all when `CRATONVM_DBG_DEFINE_STACK_FILTER`
    # is set. Kept as a note rather than deleted silently: if one of them ever
    # grows a `create_string` for a Java `StackTraceElement`, it belongs back.
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
# The shapes a bare `ObjectRef` declaration misses. `--opt` scans these too.
# They are NOT folded into the default population: they carry a reference the
# same way, but each needs a different fix (destructure and pin the inner
# reference, or pin every element), so counting them together would make the
# default number mean two things at once.
# THE TRAILING `\b` KILLED FOUR OF THE FIVE ALTERNATIVES, 2026-09-07.
#
# `Option<ObjectRef>`, `&[ObjectRef]`, `Vec<ObjectRef>` and `&[Value]` all end
# in `>` or `]`. `\b` after a NON-word character asserts that the next character
# IS a word character — and in a parameter list the next character is `,` or
# `)`. So every bracketed form failed to match, always, and `--opt` scanned only
# the bare `Value` arm while its `--help` advertised all of them.
#
# That is what let `native-builtins/src/lib.rs`'s `stream_write` /
# `stream_writeln` / `stream_writeln_inner` through: they hold `args: &[Value]`
# across `printstream_encode` (the JDK charset encoder) and
# `route_write_through_out` (`Writer.write`), the receiver relocated under
# `-XX:+UseGenerationalGC`, and `stream_fd` dereferenced the pre-call address —
# `EXCEPTION_ACCESS_VIOLATION` in `gen_heap::get_field`. Running this script
# over that file before and after the fix gave the same count both times, with
# `--opt` or without. See
# `internal/fixed-bugs/native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md`.
#
# `(?![A-Za-z_0-9])` is the assertion that was wanted: "the type ends here",
# which is true after `]` and `>` and also stops `Value` from matching
# `ValueRef`. [`assert_param_opt_alternatives_live`] runs on every invocation so
# a dead alternative can never be silent again.
PARAM_OPT = re.compile(
    r"\b([a-z_][a-z_0-9]*)\s*:\s*"
    r"(?:Option\s*<\s*(?:[A-Za-z_0-9]+::)*ObjectRef\s*>"
    r"|&\s*\[\s*(?:[A-Za-z_0-9]+::)*ObjectRef\s*\]"
    r"|Vec\s*<\s*(?:[A-Za-z_0-9]+::)*ObjectRef\s*>"
    r"|&\s*\[\s*Value\s*\]"
    r"|Value)(?![A-Za-z_0-9])"
)

# One representative signature per alternative, asserted on every run.
#
# A regex alternative that matches nothing is invisible: the scan still runs,
# still prints a total, and still looks like coverage. This is the cheapest
# possible guard against that — five `findall`s at startup — and it exists
# because the four dead arms above shipped, were used, and were reported as
# coverage for eight days.
PARAM_OPT_LIVE = [
    ("Option<ObjectRef>", "fn f(a: Option<ObjectRef>, b: i32)", "a"),
    ("&[ObjectRef]", "fn f(roots: &[ObjectRef])", "roots"),
    ("Vec<ObjectRef>", "fn f(v: Vec<ObjectRef>, x: u8)", "v"),
    ("&[Value]", "fn f(ctx: &mut dyn NativeContext, args: &[Value], t: &str) {", "args"),
    ("Value", "fn f(x: Value)", "x"),
]


def assert_param_opt_alternatives_live():
    dead = [(label, sig) for (label, sig, want) in PARAM_OPT_LIVE
            if want not in PARAM_OPT.findall(sig)]
    if dead:
        raise SystemExit(
            "PARAM_OPT has %d alternative(s) that match NOTHING — `--opt` would "
            "report coverage it does not have:\n%s"
            % (len(dead), "\n".join("  %-18s no match in: %s" % d for d in dead))
        )


assert_param_opt_alternatives_live()

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


# An `invoke_*` RHS can bind a SCALAR just as easily as a reference, and the
# `invoke` branch of REF_RHS admitted both. Seven of the twelve false positives
# in the `native-io` local tranche were this: `let limit = match
# ctx.invoke_virtual(target, "limit", "()I", ..) { Ok(Some(Value::Int(v))) => v,
# .. }` is an `i32`, and `let flushed = ctx.invoke_virtual(..).map(|_| ())` is a
# `Result<(), _>`.
#
# The discriminator is what the binding DESTRUCTURES. A statement that names a
# scalar `Value` variant and never names `Value::Object` cannot be binding a
# reference. A statement that names NEITHER (`ctx.invoke(..).ok().flatten()`)
# is left alone deliberately — that shape was a real defect in the positive
# control and no use-site test can see it.
# Match the DESTRUCTURING ARM, not any occurrence of a `Value` variant: the
# call's own arguments routinely carry `Value::Object(Some(buf))`, which made a
# whole-statement test useless — `let n = match ctx.invoke_virtual(inner,
# "read", "([BII)I", &[Value::Object(Some(buf)), ..]) { Ok(Some(Value::Int(n)))
# => n, .. }` binds an `i32` and mentions `Value::Object` in the same breath.
SCALAR_BIND = re.compile(
    # `Value::Int(v) => ..`, `Some(Value::Int(v)) => ..`,
    # `Ok(Some(Value::Int(v))) if v >= 0 => ..` — the arm may carry a GUARD, and
    # a guard contains `=` (`v >= 0`), so the span to `=>` cannot be `[^=]*`.
    r"Value::(?:Int|Long|Float|Double|Char|Short|Byte|Boolean)"
    r"\s*\(\s*[a-z_][a-z_0-9]*\s*\)[^;{}]{0,80}?=>"
    r"|\.map\s*\(\s*\|_\|\s*\(\s*\)\s*\)"
)
REF_BIND = re.compile(r"(?:Ok\s*\(\s*)?Some\s*\(\s*Value::Object")


def scalar_binding(text):
    return SCALAR_BIND.search(text) is not None and REF_BIND.search(text) is None


# A SLICE USE THAT CANNOT GO STALE. `args: &[Value]` is the shape of every
# native entry point, so "the statement mentions `args`" is not a defect — it is
# the function's argument list. Reading a SCALAR out of the slice
# (`args.get(3).and_then(|v| v.as_int())`, `matches!(args.get(1),
# Some(Value::Int(1)))`) copies an `i32` out of a `Value` that is already in
# hand; the pre-call address of a scalar is the same as its post-call one, and
# there is nothing to dereference.
#
# Only a REFERENCE pulled out of the slice can be stale, and only two shapes
# reach one:
#
#   * the statement destructures `Value::Object` out of the slice, or
#   * it passes the SLICE ITSELF to a callee, which will do that for it — the
#     shape that crashed (`stream_fd(ctx, args)` after two Java re-entries).
#
# Without this, `--opt` reported 598 `wide` candidates across the native
# crates, most of them integer reads, and a population nobody reads is the
# failure mode the 2026-08-25 write-up names.
SLICE_SCALAR = re.compile(
    r"Value::(?:Int|Long|Float|Double|Char|Short|Byte|Boolean)"
    r"|\.as_(?:int|long|i32|i64|u8|u16|u32|f32|f64|bool|char|short|byte)\s*\("
)


def slice_ref_use(name, text):
    """Does this statement reach a REFERENCE through the slice `name`?

    True when it destructures `Value::Object` from it, or hands the whole slice
    to a callee. False when the only contact is a scalar read."""
    n = re.escape(name)
    if re.search(r"Value::Object", text) and names(name, text):
        return True
    # The slice passed on as an argument: `f(ctx, args)`, `f(ctx, &args[1..])`,
    # `f(ctx, args, text)`. An INDEXED read (`args.get(2)`, `args[0]`) is not
    # this — it hands over one element, and the callee gets a `Value` by copy.
    if re.search(r"[(,]\s*&?\s*" + n + r"\s*(?:\[[^\]]*\])?\s*[,)]", text):
        if not re.search(r"\b" + n + r"\s*(?:\.\s*(?:get|first|last|iter)\s*\(|\[\s*[0-9])", text):
            return True
    # Everything else: a scalar read, or a length/emptiness test.
    if SLICE_SCALAR.search(text):
        return False
    return False


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


def names(name, text):
    """Does `text` name this binding — as itself, not as someone's FIELD?

    `process_scan_exception` binds a local `detail` and later builds an error
    from `err.detail`, a Rust struct field of a completely different value. A
    bare word-boundary match read that as a use of the local."""
    return re.search(r"(?<![.\w])" + re.escape(name) + r"\b", text) is not None


def REBIND(name):
    n = re.escape(name)
    return re.compile(
        r"\b(?:if|while)\s+let\s+[^=]*\b" + n + r"\b[^=]*="
        r"|\bfor\s+(?:\(\s*)?" + n + r"\b[^;]*\bin\b"
        r"|\|[^|]*\b" + n + r"\b[^|]*\|"
    )


Fn = collections.namedtuple("Fn", "name file line body is_test")
Stmt = collections.namedtuple("Stmt", "line text")


CHARLIT = re.compile(r"'(?:\\.|[^\\'])'")


def code_only(line):
    """Blank the CONTENTS of string and char literals; drop a trailing `//`.

    A JVM type descriptor is a bracket bomb. `"([BII)V"` carries one `(`, one
    `[` and one `)` — the `[` never closes — so a statement containing it left
    the paren/bracket depth permanently positive and NOTHING after it in that
    function ever closed a statement again. `bos_side_write_bulk` stopped after
    8 statements of 54 and its second `ObjectRef` parameter simply vanished:
    a false NEGATIVE, produced by a line that looks like nothing.

    `[B`, `()[B` and `[Ljava/lang/String;` are everywhere in a native crate, so
    this is not a corner. Blanking also stops a `//` inside a URL from eating
    the rest of a line, and keeps `;` inside a descriptor
    (`"()Ljava/lang/String;"`) from ending a statement early."""
    out, i, n = [], 0, len(line)
    while i < n:
        c = line[i]
        if c == '"':
            out.append('""')
            i += 1
            while i < n:
                if line[i] == "\\":
                    i += 2
                    continue
                if line[i] == '"':
                    i += 1
                    break
                i += 1
            continue
        if c == "/" and i + 1 < n and line[i + 1] == "/":
            break
        if c == "'":
            m = CHARLIT.match(line, i)
            if m:
                out.append("''")
                i = m.end()
                continue
        out.append(c)
        i += 1
    return "".join(out)


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
        out.append(code_only(l))
    return out


def is_test_file(lines):
    """A whole file gated by an INNER `#![cfg(test)]`.

    `test_spans` only recognises the OUTER attribute forms, so
    `native-io/src/test_support.rs` — a mock `NativeContext` whose whole point
    is that it has no GC — was scanned as production code and reported."""
    for l in lines[:40]:
        if l.strip().startswith("#![cfg(test)]"):
            return True
    return False


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


def fn_end(lines, i):
    """Last line of the fn starting at `i`, by brace matching.

    Splitting a body at "wherever the next `fn` keyword appears" is wrong two
    ways at once when a function DEFINES one: the outer body is truncated at the
    nested `fn`, so everything after it is invisible to both rules (a false
    NEGATIVE), and that tail is then attributed to the nested fn (whose
    parameter names differ, so it usually just disappears). `net_channels` was
    the 2026-08-25 pass's false positive #4 for the mirror-image reason. Brace
    matching costs nothing and removes the whole class."""
    depth, seen, n = 0, False, len(lines)
    j = i
    while j < n:
        depth += lines[j].count("{") - lines[j].count("}")
        if "{" in lines[j]:
            seen = True
        if seen and depth <= 0:
            return j
        j += 1
    return n - 1


def index(paths):
    fns = []
    for f in paths:
        raw = io.open(f, encoding="utf-8", errors="replace", newline="").read().split("\n")
        lines = strip_comments(raw)
        tspans = test_spans(lines)
        whole_file_is_test = is_test_file(lines)
        idx = [i for i, l in enumerate(lines) if FNDEF.match(l)]
        ends = {i: fn_end(lines, i) for i in idx}
        for i in idx:
            body = list(lines[i:ends[i] + 1])
            # Blank out the bodies of any fns DEFINED inside this one: they are
            # indexed in their own right, and their statements are not this
            # function's control flow.
            for k in idx:
                if i < k <= ends[i]:
                    for m in range(k - i, min(ends[k] + 1, ends[i] + 1) - i):
                        body[m] = ""
            is_test = whole_file_is_test or any(a <= i <= b for (a, b) in tspans)
            fns.append(Fn(FNDEF.match(lines[i]).group(1), f, i + 1, body, is_test))
    return fns


def signature(fn):
    """The parameter list, by PAREN matching from the `fn` line.

    `"\n".join(body[:12]).split("{")[0]` truncated any signature longer than
    twelve lines — and a `where` clause or a defaulted generic put a `{` in the
    text before the parameters ended."""
    depth, seen, out = 0, False, []
    for l in fn.body:
        out.append(l)
        depth += l.count("(") - l.count(")")
        if "(" in l:
            seen = True
        if seen and depth <= 0:
            break
    return "\n".join(out)


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
        # `buf[0]` is "" whenever a blanked comment line opened the buffer,
        # which made `starts_let` false and tore the very `let … match {}`
        # statements this guard exists to keep whole. Ask the first NON-EMPTY
        # line instead.
        first = next((b for b in buf if b), "")
        starts_let = first.startswith("let ")
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


# AN ALLOCATION ON A RETURNING ARM DOES NOT DOMINATE WHAT FOLLOWS.
#
#     let this = match args.first() {
#         Some(Value::Object(Some(r))) => *r,
#         _ => { let opt = try_alloc_synthetic(ctx, …)?;   <- allocates
#                return Ok(Some(Value::Object(Some(opt)))); }   <- and LEAVES
#     };
#     let operator = match args.get(1) { … };                   <- "use after GC"
#
# The arm that allocates is the arm that returns, so on every path that reaches
# the next statement nothing was allocated. `branchy` cannot see this: it fires
# on `return` or a bare `=>` at the START of a statement, and this statement
# starts with `let`.
#
# Four `native_stream_*` entry points, `native_inflater_input_stream_init` and
# `native_class_get_nest_members` were all reported for exactly this shape in
# the 2026-09-07 triage — a fifth of the direct, untagged tranche.
#
# CONSERVATIVE by construction: it only clears a statement when EVERY
# GC-capable token in it sits inside a brace group that returns. A statement
# with one allocation on a returning arm and another on a falling-through arm
# still counts, which is the safe direction.
RETURNING_ARM = re.compile(r"=>\s*\{[^{}]*\breturn\b[^{}]*\}|=>\s*return\b")


def alloc_only_on_returning_arm(text, allocfns):
    """Is every GC-capable token in `text` inside an arm that returns?

    Splits the statement on match arms and asks whether the ones carrying a
    GC-capable call all end in `return`. Text-level and deliberately crude:
    a wrong YES is a false negative, so it answers yes only when the statement
    has arms at all AND none of the non-returning ones is GC-capable."""
    if "=>" not in text:
        return False
    # The arms that return, blanked out; if what remains is not GC-capable, the
    # allocation lived only on paths that leave.
    stripped = RETURNING_ARM.sub("=> {}", text)
    if stripped == text:
        return False
    return not _gc_tokens(stripped, allocfns)


def _gc_tokens(text, allocfns):
    if ALLOC0.search(text):
        return True
    for c in CALLEE.findall(text):
        if c in allocfns and c not in ("if", "while", "match", "for", "return", "Some", "Ok"):
            return True
    return False


def branchy(text):
    """Does this statement sit on a path that may not reach what follows?

    The 2026-08-25 pass's false positive #3 was two `return ctx.invoke_virtual`
    calls on MUTUALLY EXCLUSIVE paths, read as one preceding the other. A
    `return` ends its path and a bare match arm is one alternative of several,
    so neither dominates a later statement.

    This TAGS (`param~`), it does not filter. That page also records a false
    negative produced by a branch-exclusivity heuristic it then deleted rather
    than improved, with the reason: a rule that dismisses confidently is worse
    than one that is noisy. Deciding exclusivity needs the read, not the grep."""
    t = text.strip()
    if t.startswith("return"):
        return True
    # `let Some(id) = reg_id else { return Err(closed_channel_exception(ctx)); };`
    # — the ONLY allocation is inside a block that leaves. Ten rows in
    # `socket_channel.rs` latched onto one of these and reported a window that
    # cannot exist, hiding whichever later call is the real one.
    if t.startswith("let ") and " else " in t and re.search(r"(?:return|break|continue)", t):
        return True
    return not t.startswith("let ") and "=>" in t


def leaves(text):
    """Every GC-capable call in this statement is on a path that LEAVES.

    Only the two forms that can be decided from the statement alone: a bare
    `return ...`, and a `let PAT = x else { ...return/break/continue... };`.
    A `match` arm is NOT included — the sibling arms fall through, and guessing
    exclusivity is the heuristic the 2026-08-25 write-up deleted rather than
    improved."""
    t = text.strip()
    if t.startswith("return"):
        return True
    return (t.startswith("let ") and " else " in t
            and re.search(r"(?:return|break|continue)", t) is not None)


# A CLOSURE DEFINITION IS NOT AN EXECUTION.
#
#     let empty = |ctx: &mut dyn NativeContext| {
#         let arr = ctx.new_ref_array(ClassId::new(0), 0);   <- allocates WHEN CALLED
#         ...
#     };
#
# Binding that closure allocates nothing. `class_annotations_by_type_impl` and
# `native_method_get_annotations_by_type` both open with one, and both were
# reported because the very next statement reads their receiver out of `args` —
# a use "after" a collection that had not happened.
#
# Only the `let NAME = |…|` form is treated this way. An immediately-invoked
# closure is written `(|| { … })()`, which does not start with `let`, and a
# closure PASSED to something that calls it (`catch_unwind(AssertUnwindSafe(||
# …))`, `map(|x| …)`) is not a `let` binding either — both still count, which is
# right, because both run before the next statement.
CLOSURE_BIND = re.compile(r"^\s*let\s+(?:mut\s+)?[a-z_][a-z_0-9]*\s*(?::[^=]*)?=\s*(?:move\s+)?\|")


def gc_capable(text, allocfns):
    if CLOSURE_BIND.match(text):
        return False
    if alloc_only_on_returning_arm(text, allocfns):
        return False
    return _gc_tokens(text, allocfns)



# ---------------------------------------------------------------------------
# LOOP RULE (`--loops`)
#
# The straight-line scan asks "is there a GC between the binding and the use",
# in STATEMENT ORDER. Inside a loop body that order is a lie: the last
# statement precedes the first on the next iteration, so a GC anywhere in the
# body stales every reference the body carries in from outside — including one
# used EARLIER in the text, and including one used in the very statement that
# does the allocating.
#
# That last case is the one that matters, because the straight-line rule
# deliberately excludes it: a use inside the same statement as the call is an
# ARGUMENT, evaluated before the call runs. True for one iteration. On the
# next, the argument is a pre-GC address.
#
# MEASURED: the 2026-09-06 `properties_sidetable.rs` defects were exactly this
# --  `for (k, v) in &snapshot { put_kv_units(ctx, this, k, v); }`, where
# `put_kv_units` reads the receiver's identity hash and so inflates a monitor.
# The default and `--any-binding` rules report 18 rows on that file before the
# fix and NONE of them is one of the four.
LOOPHEAD = re.compile(r"^\s*(?:\}\s*)?(?:for\b|while\b|loop\s*\{)")
REREAD = re.compile(r"read_native_pin|handle_get|scope\s*\.\s*get|end_blocking_region_refs")


def loop_spans(body):
    """(first, last) line index of each `for`/`while`/`loop` body. Nested loops
    yield overlapping spans, which is correct: each is its own window."""
    spans = []
    for i, l in enumerate(body):
        if not LOOPHEAD.search(l) or "{" not in l:
            continue
        depth = 0
        for j in range(i, len(body)):
            depth += body[j].count("{") - body[j].count("}")
            if j > i and depth <= 0:
                spans.append((i, j))
                break
    return spans


def scan_loops(fn, allocfns):
    stmts = statements(fn.body)
    if stmts and FNDEF.match(stmts[0].text):
        stmts = stmts[1:]
    sig = signature(fn)
    params = list(PARAM_REF.findall(sig))
    hits = []
    for (a, b) in loop_spans(fn.body):
        # EXCLUDE THE HEADER. `for x in f(ctx, this)` evaluates `f` ONCE,
        # before the body — it is not a per-iteration GC point, and counting it
        # made every `for .. in helper(ctx, this)` a row.
        inner = [st for st in stmts if a < st.line < b]
        if not inner:
            continue
        inner_txt = " ".join(st.text for st in inner)
        # Candidates: declared `ObjectRef` parameters, plus locals bound BEFORE
        # the loop. A binding made inside the loop is fresh each iteration.
        # ONLY THE LAST binding of each name before the loop. A name that is
        # re-`let` several times on the way down (`register_s2_bytebuffer`
        # shadows `this` ten times) has exactly one live binding when the loop
        # is entered; reporting all of them turned one question into thirty
        # rows and buried the two real ones in the same file.
        pre_map = {}
        for st in stmts:
            if st.line >= a:
                break
            m = LET.match(st.text)
            if m and m.group(1) != "_":
                pre_map[m.group(1)] = st
        pre = list(pre_map.items())
        cands = [(p, None) for p in params] + pre
        for name, bind in cands:
            # A handle is not an ObjectRef; that is the point of a handle scope.
            if bind is not None and re.search(
                    r"(?:scope\s*\.\s*root|handle_root|pin_native_root)\s*\(", bind.text):
                continue
            if bind is not None and not REF_RHS.search(bind.text) and not ref_use(
                    name, chr(10).join(fn.body)):
                continue
            # Refreshed or rebound INSIDE the body: this is the correct form.
            refreshed = False
            for st in inner:
                if REREAD.search(st.text) and names(name, st.text):
                    refreshed = True
                    break
                m2 = LET.match(st.text)
                if m2 and m2.group(1) == name:
                    refreshed = True
                    break
                if REBIND(name).search(st.text):
                    refreshed = True
                    break
            if refreshed:
                continue
            for st in inner:
                if not gc_capable(st.text, allocfns) or leaves(st.text):
                    continue
                if not names(name, st.text):
                    continue
                hits.append((fn.line + (bind.line if bind else 0), name,
                             fn.line + st.line, "loop"))
                break
    return hits


def scan(fn, allocfns, want_params, want_opt=False, any_binding=False):
    stmts = statements(fn.body)
    # DROP THE SIGNATURE. It reassembles as one statement, and `gc_capable`
    # reads the function's OWN NAME in it as a call — so every recursive-looking
    # signature of an allocating function made statement 0 a GC-capable call
    # and rule 2 then reported every `ObjectRef` parameter used anywhere in the
    # body. The negative control caught it: `ts_publish_real_backing_map` still
    # reported `this` AFTER the commit that pinned it.
    if stmts and FNDEF.match(stmts[0].text):
        stmts = stmts[1:]
    hits = []

    if want_params:
        sig = signature(fn)
        params = [(p, "param") for p in PARAM_REF.findall(sig)]
        if want_opt:
            params += [(p, "wide") for p in PARAM_OPT.findall(sig)
                       if p not in {n for (n, _) in params}]
        for p, shape in params:
            gc_at, gc_cond, rooted_elsewhere = None, False, False
            for k, st in enumerate(stmts):
                t = st.text
                if ROOT.search(t):
                    # Break ONLY when the rooting names THIS parameter. Rule 1
                    # was corrected for exactly this and rule 2 never inherited
                    # it: any pin anywhere cleared the whole body, so a function
                    # that pins one reference and not another read clean. The
                    # 2026-08-25 write-up calls a confident dismissal the
                    # dangerous kind of wrong, and this is that kind.
                    if re.search(r"\b" + re.escape(p) + r"\b", t):
                        break
                    rooted_elsewhere = True
                # A rebinding is a FRESH value. `let p = ...`, `if let Some(p)`,
                # `for p in ...` and a closure parameter all shadow.
                m2 = LET.match(t)
                if m2 and m2.group(1) == p:
                    break
                if REBIND(p).search(t):
                    break
                if gc_at is None:
                    if gc_capable(t, allocfns) and not leaves(t):
                        gc_at, gc_cond = k, branchy(t)
                    continue
                if names(p, t):
                    # For the `wide` tranche the mention has to REACH a
                    # reference; see `slice_ref_use`. `param` (a declared
                    # `ObjectRef`) is a reference by its type, so it keeps the
                    # bare-mention test.
                    if shape == "wide" and not slice_ref_use(p, t):
                        continue
                    kind = shape
                    if gc_cond:
                        kind += "~"
                    if rooted_elsewhere:
                        kind += "*"
                    hits.append((fn.line + stmts[gc_at].line, p,
                                 fn.line + st.line, kind))
                    break

    for i, st in enumerate(stmts):
        m = LET.match(st.text)
        if not m:
            continue
        name = m.group(1)
        if name == "_":
            continue
        # DEFAULT: the binding's own RHS must be GC-capable. That is not the
        # defect's definition — it is a proxy for "this local names a fresh
        # object" — and it costs real recall: `fd_obj` in the pre-fix
        # `native_fcimpl_open`, bound by `match args.first()`, is one of the
        # seven references that fix had to root, and this line is why the rule
        # reports six. `--any-binding` drops the requirement; the type filter
        # below still applies.
        if not any_binding and not gc_capable(st.text, allocfns):
            continue
        # A binding whose RHS ROOTS something is a HANDLE (an opaque slot), not
        # an `ObjectRef`. Handles are exactly what cannot go stale — that is the
        # point of the handle scope — so a later use of one is not this defect.
        if re.search(r"\b(?:scope\s*\.\s*root|handle_root|pin_native_root)\s*\(", st.text):
            continue
        if not REF_RHS.search(st.text) and not ref_use(name, "\n".join(fn.body)):
            continue
        if scalar_binding(st.text) and not ref_use(name, "\n".join(fn.body)):
            continue
        gc_at = None
        gc_cond = False
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
                # A GC on a path that LEAVES does not precede what follows it,
                # so keep looking rather than latching. This is not a
                # branch-exclusivity guess: the statements below are reached
                # only when that branch did not run.
                if gc_capable(t, allocfns) and not leaves(t):
                    gc_at, gc_cond = k, branchy(t)
                continue
            if names(name, t):
                kind = "local"
                if gc_cond:
                    kind += "~"
                if rooted_elsewhere:
                    kind += "*"
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
    ap.add_argument("--any-binding", action="store_true", dest="any_binding",
                    help="rule 1: do not require the BINDING statement to be GC-capable")
    ap.add_argument("--loops", action="store_true",
                    help="a GC anywhere in a loop body stales every reference carried in")
    ap.add_argument("--opt", action="store_true",
                    help="also scan Option<ObjectRef> / &[ObjectRef] / Vec<ObjectRef> / &[Value] / Value parameters (`wide`). `args: &[Value]` is the shape of every registered native, so this is the tranche that covers native ENTRY "
                    "points rather than their helpers.")
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
        if a.loops:
            found = scan_loops(fn, allocfns)
        else:
            found = scan(fn, allocfns, True, a.opt, a.any_binding)
        for (ln, nm, use, kind) in found:
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
