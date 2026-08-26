# ✅ RESOLVED — 48 natives held an unpinned reference across a GC-capable call

## Status

**RESOLVED 2026-08-25.** Two search rules, 48 sites fixed, both rules now
report zero over `native-builtins/src`. `cargo test -p cratonvm-native-builtins`:
**4153 passed, 0 failed**; no new rustfmt diffs across the 18 touched files.

Every one of the 48 was **read before it was touched**. That was not caution
for its own sake — see "What the rules got wrong", where they are wrong in five
distinct ways, one of which would have made me skip a real defect and record it
as dismissed.

## The shape

> a native holds a raw `ObjectRef` / `Value::Object` across a call that can
> allocate, and dereferences it afterwards.

The caller's operand slot is a root and gets remapped; the native's copy is not.
A collection inside that window leaves the copy stale, and the next read sees an
all-zero header — which the class manager names `java.lang.Object`. That is the
whole of `d != java.lang.Object`, the failure in
`bug-generational-ntru-unpinned-jit-reference-20260821-FIXED.md`.

## The two rules

| rule | what it finds | raw | after filters | fixed |
|---|---|---|---|---|
| **bound local** — object-typed local bound before a GC-capable call, used after, body has no pin | `format_impl`-style | 1074 | 8 | 8 |
| **receiver** — 2+ `ctx.invoke_*` on the same receiver in one scope with no `read_native_pin` between (`ea5ed2704`) | parameters and receivers, which rule 1 structurally cannot see | 51 | 40 | 40 |

Rule 2 exists because rule 1 has a blind spot rule 1 cannot close: its premise
is a `let` binding, and **a function parameter is never `let`-bound**. A helper
that takes a receiver and drives its Java is the normal factoring here, so the
parameter case is the commoner one.

## What the rules got wrong

**False positives, four distinct kinds.** Each looked exactly like a defect:

1. **Already pinned, different spelling.** `fjp_compute_void` pins before both
   calls and re-derives after — the pin-before-all form. The rule wanted the
   re-derive *between* the calls. I nearly "fixed" correct code.
2. **Shadowed identifiers.** `reflect_annotations` had two
   `invoke_virtual(level, …)` calls that are *distinct bindings in separate
   match arms* — different objects. The rule matches names, not bindings.
3. **Tail-call returns.** `huc_get_input_stream`'s two calls are both
   `return ctx.invoke_virtual(...)` on mutually exclusive paths. Rule 1 filtered
   these; rule 2 never inherited the filter.
4. **Nested `fn` scopes.** `net_channels` had one call in a closure and one in a
   nested `fn` 20 lines later. The scope splitter broke on `|ctx, args|` but not
   on `fn`.

**One false NEGATIVE, which is the dangerous kind.** `lucene_es` was tagged
"likely if/else EXCLUSIVE" by a crude rule — any `} else {` between the two
calls. That `else` belonged to an unrelated ternary; both calls run, with an
allocating `lucene_static_object` between them. Trusting the tag would have left
a real defect in place *and* recorded it as dismissed. **The tag is removed, not
improved** — a heuristic that produces confident dismissals is worse than one
that produces noise.

**Real defects the rules never saw**, found only by reading around a flagged
line:

* `reflect_annotations` — `record_level`, read before an `intValue()` call and
  dereferenced after it. The flagged pair was the false positive; the defect was
  two lines away.
* `lang_string`'s Calendar arm — `obj` passed to `zoned_fields` after an
  invoke, inside a span the rule reported as one 1906-line block that is
  actually three unrelated things sharing an identifier.

## The fix is not mechanical either

**I introduced a pin leak while fixing this.** My first `deprecated_io_util`
patch put `pin_native_root(inner)` *inside* a `for i in 0..len` loop: one pin
per iteration, never released, accumulating roots for the whole read. Caught
only because the indentation looked wrong and I checked. Correct shape for a
loop is **pin once outside, re-derive per use inside** — used by every loop fix
here (`jmx`, `service_loader`, `streams`, `phases_late`, `xml_stax`,
`properties_sidetable`, `t3_impl`).

## Awareness is not coverage

Two sites were written by someone who clearly knew this hazard:

* `register_p61_logging` carries a `GC SAFETY` comment explaining that the
  absorbed throwable is deliberately reported *before* `close()` rather than
  held across it — and still left `stream` itself unpinned across `flush()`.
* `messaging_shims` pins `info`, derives `info_now`, calls `getSelector()`, and
  never re-derives before using `info_now` after it.

This is also why the audit **under-reports**: any pin anywhere in a body clears
that body for rule 1, so a native that pins one reference and not another reads
clean.

## If this is worth a gate

Start from the **corrected** rule, not the one this page opened with: skip
already-pinned receivers, skip tail-call returns, treat a nested `fn` as its own
scope, and do not attempt branch-exclusivity. Re-running the corrected rule
after the 47 found exactly **one** more — a genuine one — which is the evidence
that the corrections narrowed nothing real.

A real gate wants dataflow over the `ctx.invoke_*` sites, with these two
legitimate patterns allowlisted: `unsafe_natives.rs` pins the AQS blocker for
retention only across `park()` and never dereferences it, and
`graalvm_compat.rs` pins `ImageSingletons` permanently by design.

The heuristics are deliberately not checked in. Something with a 1074-to-8
ratio should not look like a test.

## What this does NOT establish

**No dynamic proof.** All 48 are *structurally* identical to a defect that was
proven; none was observed failing. The pins are correct regardless — pinning
across a call that can move the object is right whether or not a workload
currently hits it — but this is not "48 live bugs found".

The per-candidate discriminator, if one is ever suspected dynamically, is the
lever from the parent record: `CRATONVM_MOVING_YOUNG_NO_JIT=1` makes the
collector decline to move without changing anything else, so fails-without /
passes-with is the signature.
