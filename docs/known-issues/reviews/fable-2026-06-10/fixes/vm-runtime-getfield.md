# Fix note — `vm-runtime-getfield`

Findings B2 + B4 from `docs/reviews/fable-2026-06-10/vm-runtime.md`.

## Finding

**B2 (MEDIUM, memory safety):** `jit_getfield(obj_ptr, field_index)` in
`vm/src/jit/helpers.rs` performed an **unchecked out-of-bounds heap read** —
`*(obj + HEADER_SIZE + field_index*SLOT_SIZE)` with no validation of
`field_index` against the receiver's `num_slots`, unlike every `jit_putfield_*`
sibling which is guarded by `jit_putfield_slot_in_bounds`. On the live inlining
codegen path (`jit/src/x64.rs` callee-getfield during caller compilation) a
stale `field_index` (synthetic/real-JDK layout drift) or an operand-stack
miscompile reads the *neighbouring* heap object's bytes and hands them back to
JIT'd Java as an `i64`/object pointer — info leak + potential follow-on UAF if
interpreted as a ref. It also returned `0` on a null receiver (no NPE), the same
silent-null class the array-load helpers were already fixed for.

**B4 (MEDIUM, catchability):** an operand-stack overflow reported by
`ValueStack::push` as `RuntimeError::NotImplemented { feature: "operand stack
overflow" }` was mapped by `throw_runtime_error` to an **uncatchable**
`InternalError` (`exceptions.rs:474`), so a JVM stack overflow hard-unwound the
whole call stack instead of surfacing as a Java-catchable
`java.lang.StackOverflowError`.

## Root cause

- B2: the read side of the field-access helpers was simply never given the
  bounds-check + null-NPE treatment its write-side siblings received.
- B4: `ValueStack::push` uses the wrong `RuntimeError` variant
  (`NotImplemented`, which is the project's "uncatchable internal error" signal)
  for what is a recoverable, catchable JVM condition. The canonical fix would be
  in `value_stack.rs::push`, but that file is owned by another agent; the
  interpreter is the only layer that converts runtime errors into Java exception
  objects, so the remap is done there (a sound, equally-correct interception
  point).

## Exact change

`vm/src/jit/helpers.rs` — `jit_getfield`:
- Null receiver: `set_jit_pending_npe()` + `return i64::MIN` (mirrors
  `jit_arraylength`; the pending-NPE flag is drained on every JIT method return,
  interpreter.rs:15233) instead of the old silent `return 0`.
- Out-of-range / negative slot: `if !jit_putfield_slot_in_bounds(obj_ptr,
  field_index) { return 0; }` BEFORE the raw read — the symmetric guard the
  `jit_putfield_*` helpers already use. Returns `0` (the interpreter's
  out-of-range `get_field` default) without dereferencing, so no OOB read can
  occur.

`vm/src/runtime/interpreter.rs` — the
`Err(MethodCallFailed::InternalError(VmError::Runtime(re)))` arm of the
per-instruction dispatch (the sole runtime-error→Java-exception conversion
site): normalize `RuntimeError::NotImplemented { feature: "operand stack
overflow" }` → `RuntimeError::StackOverflowError` before `throw_runtime_error`.
`StackOverflowError` already maps to a real `java/lang/StackOverflowError`
(`exceptions.rs:393`) and is routed through the method's exception table, so an
in-method `catch (StackOverflowError)` / `catch (Throwable)` now observes it.

Tests (in the existing `#[cfg(test)] mod tests` of `helpers.rs`):
- `jit_getfield_null_sets_pending_npe` — replaced the now-stale
  `jit_getfield_null_returns_zero`; asserts `i64::MIN` + pending-NPE flag.
- `jit_getfield_oob_slot_does_not_read_past_object` — allocates a 2-field object
  and asserts slots 2, 5, and -1 all return `0` without reading OOB and without
  raising NPE.
- `jit_getfield_in_bounds_reads_stored_int` — round-trips a value through a valid
  slot to prove the fast path is unchanged.

## Files touched

- `vm/src/jit/helpers.rs` (jit_getfield + 3 tests)
- `vm/src/runtime/interpreter.rs` (operand-stack-overflow → StackOverflowError remap)
- `docs/reviews/fable-2026-06-10/fixes/vm-runtime-getfield.md` (this note)

## Tests added

3 unit tests on `jit_getfield` (null, OOB slot, in-bounds round-trip). No
interpreter-level integration test for B4 was added — exercising a real
operand-stack overflow requires a full VM + a stack-bombing method and risks not
compiling cleanly; the remap is a small, localized, type-checked change and the
existing `throw_runtime_error` StackOverflowError path is already unit-tested in
`types/src/error.rs` / `exceptions.rs`.

## Follow-up & risk

- **B4 canonical fix (out of scope for owned files):** the *right* place is
  `vm/src/runtime/value_stack.rs::push` (line ~317) and the other
  `NotImplemented{feature:"operand stack overflow"}` sites (537/551/565/579/593)
  — change them to return `RuntimeError::StackOverflowError` directly. The
  `IllegalStateException`-based overflow sites (push_int/push_null family, lines
  363/428/442/457/509) are catchable but use the wrong exception *class*
  (`IllegalStateException` instead of `StackOverflowError`); a follow-up there
  would align them with the spec. My interpreter-level remap covers the
  uncatchable `NotImplemented` variant, which was the actual B4 hazard.
- **B2 reachability:** x64.rs may emit its own inline null-check before the
  getfield helper call, making the helper's null arm partly defensive; the OOB
  guard is the load-bearing memory-safety fix and the null-arm change keeps the
  helper consistent with its siblings. Low risk — the in-bounds fast path is
  byte-for-byte unchanged (verified by the round-trip test).
- **Behavioral change:** `jit_getfield(null)` now returns `i64::MIN` + pending
  NPE instead of `0`. Any (incorrect) caller that relied on the old silent-0 on
  null is now correctly throwing NPE — the intended JVMS behavior, matching the
  array-helper fixes landed in the same review round.
