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
    # `declared_fields`, `declared_methods`, `class_annotations` and
    # `record_components` are NOT here. All four are `&self` methods on
    # `vm_exec` that take the class-manager read lock and build a Rust `Vec` of
    # metadata — no Java object is allocated and no bytecode runs. Listing them
    # made a reflective metadata read look like a collection point:
    # `lk_member_access_flags` and `uri_has_synthetic_layout` rooted 16 rows of
    # the transitive tranche between them. Same class of error as
    # `capture_stack_trace` and `get_ascii_case_string_cached`, both removed
    # earlier for the same reason — a getter named like a producer.
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
# A DESTRUCTURING `let` IS A REBINDING TOO, and `LET` cannot see one.
# `rooted_across1` hands its refreshed receiver back as `(this, out)`, and
# all 21 of its callers spell that `let (this, buf) = rooted_across1(..)`.
# With only the scalar `LET`, none of those reads as a rebinding and every
# one of them was reported as a caller reusing a stale copy -- 21 of the
# `--launder` rule's 34 native-collections sites, all false.
LET_PAT = re.compile(r"^\s*let\s+(.*?)\s*=[^=]")
IDENT = re.compile(r"[a-z_][a-z_0-9]*")
PAT_KEYWORDS = frozenset(("mut", "ref", "if", "let"))


def let_binds(name, text):
    """True if this statement `let`-binds `name` -- scalar OR destructured.

    The pattern side of a `let` is matched loosely on purpose: a name that
    appears there is bound by it in every shape this tree uses (`(a, b)`,
    `[a, b]`, `Some(a)`, `Foo { a, .. }`), and a name that appears on the
    pattern side but is NOT a binder (an enum path segment, say) is a
    conservative early stop rather than a false report."""
    m = LET.match(text)
    if m and m.group(1) == name:
        return True
    m = LET_PAT.match(text)
    if not m:
        return False
    pat = m.group(1)
    if not pat.startswith(("(", "[", "{")) and "(" not in pat and "{" not in pat:
        return False
    return any(t == name for t in IDENT.findall(pat) if t not in PAT_KEYWORDS)
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


# A BARE NAME THAT EVERY TYPE IMPLEMENTS IS NOT A CALL GRAPH EDGE.
#
# `allocating()` keys the graph on the bare identifier before `(`, so all forty
# `fn drop(&mut self)` bodies in these crates collapse into ONE node — and one
# of them, the TLS guard in `t27_tls.rs`, calls `end_blocking_region`. That made
# the node `drop` a depth-0 allocator, and every `drop(guard)` / `drop(map)` /
# `drop(registry)` in the tree inherited it: 65 rows of the transitive tranche,
# all of them dropping a mutex guard or a hash map.
#
# Excluding them is sound rather than merely convenient. A blocking region's
# HAZARD is the window between `begin_blocking_region` and `end_blocking_region`,
# and the audit matches both tokens directly wherever they appear; the `drop`
# that closes the guard is the END of a window it has already reported.
#
# `get`, `new`, `build`, `finish` and `call` are deliberately NOT here. They
# collide too, but in these crates they are also the names of real helpers that
# really do allocate, and dropping them would trade a false positive for a false
# negative — the direction this file's history says not to take.
UNRESOLVABLE_BY_NAME = frozenset("""
drop next clone fmt from into default eq ne hash cmp partial_cmp
deref deref_mut as_ref as_mut borrow borrow_mut to_owned clone_from
to_string try_from try_into len is_empty iter into_iter
""".split())


def allocating(fns, depth):
    alloc = {fn.name: 0 for fn in fns if ALLOC0.search("\n".join(fn.body))}
    callees = [(fn.name, set(CALLEE.findall("\n".join(fn.body))) - UNRESOLVABLE_BY_NAME)
               for fn in fns]
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
        if c in UNRESOLVABLE_BY_NAME:
            continue
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


EXITS = re.compile(r"^(?:return\b|continue\b|break\b)")


def stmt_depths(body, stmts):
    """Brace depth at the START and at the END of each statement.

    `statements()` already tore the body apart on `;` and on bare braces, so a
    block's contents are SEPARATE statements from the `if` that opened it. That
    is why the returning-arm rule cannot reach this case: there is no single
    statement to blank. Depth is what recovers the nesting.

    BOTH ends are needed, and the first version had only starts. A statement
    that is just `}` starts at the INNER depth, so a walk looking for "the
    depth came back down" never saw the block close and concluded the use was
    inside it — `native_printstream_flush` kept reporting, matched against a
    `return`ing block three levels up. The END depth is where a block closes."""
    depth_at_line, d = [], 0
    for l in body:
        depth_at_line.append(d)
        d += l.count("{") - l.count("}")
    depth_at_line.append(d)
    starts, ends = [], []
    for i, st in enumerate(stmts):
        a = st.line
        b = stmts[i + 1].line if i + 1 < len(stmts) else len(body)
        starts.append(depth_at_line[a] if a < len(depth_at_line) else 0)
        ends.append(depth_at_line[b] if b < len(depth_at_line) else depth_at_line[-1])
    return starts, ends


def dominates(stmts, depths, k, j):
    """Does statement `k` run on every path that reaches statement `j`?

    Only one shape is answered NO, and only when it is structurally certain:
    `k` sits inside a block DEEPER than `j`, that block closes before `j`, and
    its last statement is an unconditional exit.

        if cond {
            let s = ctx.create_string(..);   <- k, depth 1
            return Ok(None);                 <- the block LEAVES
        }
        let x = args.get(1);                 <- j, depth 0

    `native_printstream_flush`, `native_class_for_name` and
    `native_printwriter_write_string` are all this, and all three were reported
    on a Java re-entry the reporting path cannot reach.

    Everything else answers YES, including a block that merely MIGHT exit — a
    confident dismissal is the dangerous kind, and this file's own history says
    so twice."""
    starts, ends = depths
    dk, dj = starts[k], starts[j]
    if dk == 0:
        return True
    # Walk OUTWARD from `k`'s own innermost block, one level at a time, asking
    # each enclosing block whether it exits before `j`. The first version
    # scanned the whole span from `k` down to `j`'s level in one go, which
    # swept in SIBLING blocks: `native_printstream_flush`'s allocation sits in
    # a block that returns, but two later sibling blocks do not, and their last
    # statement is what the scan found. Only the blocks that CONTAIN `k` decide
    # whether `k` reaches `j`.
    # DEPTH ALONE CANNOT COMPARE SIBLINGS. `native_class_for_name`'s allocation
    # and its use are BOTH at depth 2 — in two different blocks, one of which
    # returns — so a `dk <= dj` shortcut answered "reaches" on a path that does
    # not exist. What decides it is whether a block CONTAINING `k` closes
    # before `j`, at any depth, and whether that block exits.
    pos, cur = k, dk
    while cur > 0:
        m = pos
        while m < j and ends[m] >= cur:
            m += 1
        if m >= j:
            # This block is still open at the use, so `j` is inside it and `k`
            # precedes it on the same path.
            return True
        last = None
        for t in range(pos, m + 1):
            txt = stmts[t].text.strip()
            if not txt or txt in ("{", "}"):
                continue
            if starts[t] >= cur:
                last = txt
        if last and EXITS.match(last):
            return False
        pos, cur = m, ends[m]
    return True


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
                if let_binds(name, st.text):
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


# `dominates` DISMISSES, which is the dangerous direction, so it proves itself
# on every run — both halves. A rule that only demonstrated the dismissal would
# pass while dismissing everything, and this file's own history records exactly
# that failure mode ("a rule that dismisses confidently is worse than one that
# is noisy").
#
# Each case is a function body; the assertion names the statement that
# allocates and the statement that uses, and says whether the first reaches the
# second.
DOMINANCE_CASES = [
    # The block that allocates RETURNS: it cannot reach the later statement.
    ("if-returns", False, [
        "fn f(ctx: &mut dyn NativeContext, args: &[Value]) {",
        "    if let Some(x) = args.first() {",
        "        let s = ctx.create_string(\"x\");",
        "        return;",
        "    }",
        "    let y = args.get(1);",
        "}",
    ]),
    # The same block WITHOUT the return falls through: it does reach it.
    ("if-falls-through", True, [
        "fn f(ctx: &mut dyn NativeContext, args: &[Value]) {",
        "    if let Some(x) = args.first() {",
        "        let s = ctx.create_string(\"x\");",
        "    }",
        "    let y = args.get(1);",
        "}",
    ]),
    # SIBLING blocks at the SAME depth: the allocation returns, the use is in a
    # later block. Depth comparison alone answers this one wrong.
    ("sibling-blocks", False, [
        "fn f(ctx: &mut dyn NativeContext, args: &[Value]) {",
        "    if a {",
        "        let s = ctx.create_string(\"x\");",
        "        return;",
        "    }",
        "    if b {",
        "        let y = args.get(1);",
        "    }",
        "}",
    ]),
    # Straight line, no blocks at all: always reaches.
    ("straight-line", True, [
        "fn f(ctx: &mut dyn NativeContext, args: &[Value]) {",
        "    let s = ctx.create_string(\"x\");",
        "    let y = args.get(1);",
        "}",
    ]),
]


def assert_dominance_both_ways():
    for name, want, body in DOMINANCE_CASES:
        stmts = statements(body)
        if stmts and FNDEF.match(stmts[0].text):
            stmts = stmts[1:]
        depths = stmt_depths(body, stmts)
        k = next((i for i, st in enumerate(stmts) if "create_string" in st.text), None)
        j = next((i for i, st in enumerate(stmts) if "args.get(1)" in st.text), None)
        if k is None or j is None or not k < j:
            raise SystemExit(
                "dominance self-test %r no longer has an alloc before a use — the "
                "case has rotted and proves nothing" % name)
        got = dominates(stmts, depths, k, j)
        if got != want:
            raise SystemExit(
                "dominance self-test %r: expected reaches=%s, got %s. `dominates` "
                "decides whether a reported window can exist; a wrong NO hides a "
                "real defect." % (name, want, got))


assert_dominance_both_ways()


# ---------------------------------------------------------------------------
# LAUNDERING RULE (`--launder`)
#
# A refresh contract expressed as a PARAMETER MODE only binds the immediate
# call. `ordered_snapshot_kv(obj: &mut ObjectRef)` says so on itself:
#
#     "Taking `obj` by `&mut` is the point: it forces every caller's own
#      receiver to be refreshed across the walk instead of silently carrying a
#      pre-GC address into the pins and virtual dispatches that follow."
#
# A wrapper that takes the SAME receiver BY VALUE satisfies that `&mut` with a
# COPY -- `fn helper(this: ObjectRef) { let mut this = this; api(ctx, &mut
# this); }` -- and throws the refreshed address away at the return, while its
# caller walks on with the pre-GC one. No rule scoped to a single function can
# see it: the refresh looks correct in the callee and the staleness is in the
# caller.
#
# MEASURED: `collect_own_property_names` was exactly this, and the receiver its
# caller then read `defaults` out of was the SIGSEGV of 2026-09-06.
PARAM_BYVAL = re.compile(r"([a-z_][a-z_0-9]*)\s*:\s*ObjectRef\b")
REFRESH_API = re.compile(r"read_native_pin|handle_get|scope\s*\.\s*get\b")


def _params(sig):
    """Parameter names in declaration order, top-level commas only."""
    i = sig.find("(")
    if i < 0:
        return []
    depth, buf, out = 0, [], []
    for ch in sig[i + 1:]:
        if ch in "([<":
            depth += 1
        elif ch in ")]>":
            if depth == 0:
                break
            depth -= 1
        if ch == "," and depth == 0:
            out.append("".join(buf))
            buf = []
        else:
            buf.append(ch)
    if buf:
        out.append("".join(buf))
    names = []
    for p in out:
        m = re.match(r"\s*(?:mut\s+)?([a-z_][a-z_0-9]*)\s*:", p)
        names.append(m.group(1) if m else "")
    return names


def _args(text, callee):
    """Top-level argument expressions of the FIRST `callee(..)` in `text`."""
    m = re.search(r"(?<![a-z_0-9.])" + re.escape(callee) + r"\s*\(", text)
    if not m:
        return None
    depth, buf, out = 0, [], []
    for ch in text[m.end():]:
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            if depth == 0:
                break
            depth -= 1
        if ch == "," and depth == 0:
            out.append("".join(buf).strip())
            buf = []
        else:
            buf.append(ch)
    if buf:
        out.append("".join(buf).strip())
    return out


def return_type(fn):
    """The declared return type, or "" for a unit function.

    Needed because "hands the refresh back" is only meaningful for a function
    that returns something. `ad_grow`, `lbq_ensure_capacity`,
    `lli_resnapshot` and `pq_ensure_capacity` all end on a trailing
    `if .. { .. }` block that MENTIONS `this`, and a tail test without this
    gate read all four as returning it. They return `()`."""
    sig = signature(fn)
    head = "\n".join(fn.body[: len(sig.split("\n")) + 4])
    depth, idx = 0, None
    for i, ch in enumerate(head):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                idx = i
                break
    if idx is None:
        return ""
    rest = head[idx + 1:]
    brace = rest.find("{")
    if brace >= 0:
        rest = rest[:brace]
    m = re.search(r"->\s*(.+)", rest, re.S)
    return " ".join(m.group(1).split()) if m else ""


def returns_param(fn, p):
    """True if `fn`'s TAIL EXPRESSION hands `p` back to its caller.

    Only the tail, deliberately. `cslm_ensure_capacity` has an early
    `return (keys, values);` on the no-growth path and returns
    `(new_keys, new_values)` on the growth path -- the one that refreshes
    `keys`/`values` and then does NOT return them. Accepting any `return` that
    names the parameter excused exactly that laundering."""
    if not return_type(fn):
        return False
    stmts = statements(fn.body)
    if stmts and FNDEF.match(stmts[0].text):
        stmts = stmts[1:]
    for st in reversed(stmts):
        t = st.text.strip()
        if not t or t in ("}", "};"):
            continue
        # A statement is not a tail expression, and neither is a `let`.
        if t.endswith(";") or LET.match(t) or LET_PAT.match(t):
            return False
        return names(p, t)
    return False


def launderers(fns):
    """Functions that take an `ObjectRef` BY VALUE and refresh their own copy."""
    out, out_returns = {}, {}
    for fn in fns:
        sig = signature(fn)
        body = "\n".join(fn.body)
        for p in set(PARAM_BYVAL.findall(sig)):
            # `&mut ObjectRef` also matches the bare pattern; that one is fine,
            # it is the contract being honoured.
            if re.search(r"\b" + re.escape(p) + r"\s*:\s*&mut\s+ObjectRef\b", sig):
                continue
            why = None
            if re.search(r"&mut\s+" + re.escape(p) + r"\b", body):
                why = "passes `&mut` of its own copy to a refreshing API"
            # NOT reported: `let this_pin = pin(this); .. this = read(this_pin,
            # this)` -- a function refreshing its OWN copy for its OWN body is
            # the correct, ubiquitous idiom, and nothing is being laundered.
            # It fired 244 times in one crate, which is what said it was the
            # wrong question. The laundering shape is narrower: a contract that
            # says "this refreshes YOUR variable", satisfied with a copy.
            # NOT laundering: the copy is RETURNED. `rooted_across1` exists to
            # root one receiver across a body and hand it back as
            # `(this, out)`, and every caller spells the call
            # `let (this, buf) = rooted_across1(..)` -- a refresh, not a leak.
            # Reported on its own line rather than dropped, because "returns
            # it" only helps a caller that BINDS it: `let (_, buf) = ..` is
            # back in the laundering shape and this rule cannot tell.
            if why and returns_param(fn, p):
                out_returns.setdefault(fn.name, []).append(p)
                why = None
            if why:
                out.setdefault(fn.name, []).append((p, why, fn))
    return out, out_returns


def scan_launder(fns):
    """(laundering fn, param) plus the call sites whose caller reuses its own
    copy after the call -- which is where the stale read actually happens."""
    lau, handed_back = launderers(fns)
    hits = []
    for name, entries in sorted(lau.items()):
        for p, why, fn in entries:
            idx = _params(signature(fn)).index(p) if p in _params(signature(fn)) else -1
            sites = []
            for g in fns:
                if g.name == name:
                    continue
                stmts = statements(g.body)
                for k, st in enumerate(stmts):
                    if not re.search(r"(?<![a-z_0-9.])" + re.escape(name) + r"\s*\(", st.text):
                        continue
                    args = _args(st.text, name)
                    if not args or idx < 0 or idx >= len(args):
                        continue
                    a = args[idx].strip()
                    if not re.fullmatch(r"[a-z_][a-z_0-9]*", a):
                        continue
                    # `let (this, buf) = helper(ctx, this, ..)` REBINDS the
                    # very name it passes, so every statement below reads the
                    # RETURNED value. The rebinding scan starts at k+1 and
                    # structurally cannot see the call statement itself.
                    if let_binds(a, st.text):
                        continue
                    for st2 in stmts[k + 1:]:
                        # A REBINDING is a fresh value, not a stale use. A
                        # registration function holds several closures, each
                        # with its own `let this = obj_arg(args, 0)?`, and
                        # without this every later closure read as a reuse of
                        # the earlier one's receiver.
                        if let_binds(a, st2.text) or REBIND(a).search(st2.text):
                            break
                        if names(a, st2.text):
                            sites.append((g.name, a, g.line + st.line, g.line + st2.line))
                            break
            hits.append((name, p, why, sites))
    return hits, handed_back


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
    depths = stmt_depths(fn.body, stmts)
    hits = []

    if want_params:
        sig = signature(fn)
        params = [(p, "param") for p in PARAM_REF.findall(sig)]
        if want_opt:
            params += [(p, "wide") for p in PARAM_OPT.findall(sig)
                       if p not in {n for (n, _) in params}]
        for p, shape in params:
            # CANDIDATES, not "the first one". A GC-capable statement that sits
            # in a block which returns does not reach the use, so the use has
            # to pick the first candidate that DOMINATES it rather than being
            # matched against whichever came first. See [`dominates`].
            gc_cands, rooted_elsewhere = [], False
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
                if let_binds(p, t):
                    break
                if REBIND(p).search(t):
                    break
                # USE FIRST, then record. A statement can both allocate
                # and name the parameter, and it is a USE only against
                # candidates STRICTLY BEFORE it — its own call's arguments are
                # evaluated before that call runs, which is this file's
                # "STATEMENTS, NOT LINES" rule. Recording it afterwards keeps
                # it available to the statements that follow.
                use_hit = names(p, t)
                if use_hit:
                    gc_at = next((g for g in gc_cands
                                  if dominates(stmts, depths, g, k)), None)
                    if gc_at is None:
                        use_hit = False
                    else:
                        gc_cond = branchy(stmts[gc_at].text)
                if not use_hit:
                    if gc_capable(t, allocfns) and not leaves(t):
                        gc_cands.append(k)
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
        gc_cands = []
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
            if let_binds(name, t):
                break
            if REBIND(name).search(t):
                break
            # USE FIRST, then record — see the parameter loop above.
            use_hit = names(name, t)
            if use_hit:
                gc_at = next((g for g in gc_cands
                              if dominates(stmts, depths, g, k)), None)
                if gc_at is None:
                    use_hit = False
                else:
                    gc_cond = branchy(stmts[gc_at].text)
            if not use_hit:
                if gc_capable(t, allocfns) and not leaves(t):
                    gc_cands.append(k)
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
    ap.add_argument("glob", nargs="+")
    ap.add_argument("--depth", type=int, default=6)
    ap.add_argument("--detail", action="store_true")
    ap.add_argument("--tests", action="store_true", help="include test bodies")
    ap.add_argument("--only", default=None)
    ap.add_argument("--any-binding", action="store_true", dest="any_binding",
                    help="rule 1: do not require the BINDING statement to be GC-capable")
    ap.add_argument("--launder", action="store_true",
                    help="helpers that take ObjectRef BY VALUE and refresh their own copy")
    ap.add_argument("--loops", action="store_true",
                    help="a GC anywhere in a loop body stales every reference carried in")
    ap.add_argument("--opt", action="store_true",
                    help="also scan Option<ObjectRef> / &[ObjectRef] / Vec<ObjectRef> / &[Value] / Value parameters (`wide`). `args: &[Value]` is the shape of every registered native, so this is the tranche that covers native ENTRY "
                    "points rather than their helpers.")
    a = ap.parse_args()
    # `**` is written in every invocation in the docs; without recursive=True
    # glob treats it as a single `*` and SILENTLY drops every file below the
    # first subdirectory level -- 41 of native-builtins' 178 sources.
    files = sorted({f for g in a.glob for f in glob.glob(g, recursive=True)})
    fns = index(files)
    allocfns = allocating(fns, a.depth)
    rows, skipped = [], 0
    for fn in fns:
        if fn.is_test and not a.tests:
            skipped += 1
            continue
        if a.only and fn.name != a.only:
            continue
        if a.launder:
            found = []
        elif a.loops:
            found = scan_loops(fn, allocfns)
        else:
            found = scan(fn, allocfns, True, a.opt, a.any_binding)
        for (ln, nm, use, kind) in found:
            rows.append((os.path.basename(fn.file), ln, fn.name, nm, use, kind))
    if a.launder:
        hits, handed_back = scan_launder(fns)
        print("laundering helpers: %d" % len(hits))
        live = 0
        for name, p, why, sites in hits:
            mark = "  <-- callers reuse their copy" if sites else ""
            print("  %-46s %-14s %s%s" % (name, p, why, mark))
            for g, var, cl, ul in sites:
                live += 1
                print("        caller %-40s passes `%s` at :%d, uses it again at :%d" % (g, var, cl, ul))
        print("TOTAL laundering helpers: %d ; call sites that then reuse: %d" % (len(hits), live))
        if handed_back:
            print("not laundering -- the refreshed copy is RETURNED "
                  "(sound only where the caller binds it): %d"
                  % sum(len(v) for v in handed_back.values()))
            for nm, ps in sorted(handed_back.items()):
                print("  %-46s %s" % (nm, ", ".join(sorted(ps))))
        return
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
