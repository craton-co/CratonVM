# Fix vm-runtime-soe — B4: operand-stack overflow → catchable `StackOverflowError`

**Report:** `docs/reviews/fable-2026-06-10/vm-runtime.md` §B4 (MEDIUM, carry-over)
**Owned file:** `vm/src/runtime/value_stack.rs`
**Status:** DONE

## Problem (B4)

`ValueStack::push` (and the type-specialised hot-path pushes `push_int`,
`push_long`, `push_float`, `push_double`, `push_null`) returned
`RuntimeError::NotImplemented { feature: "operand stack overflow" }` on a full
operand stack.

`NotImplemented` is mapped to an **uncatchable** VM-internal error
(`exceptions.rs` → `MethodCallFailed::InternalError`) AND is explicitly excluded
from the runtime-error → Java-exception conversion in
`interpreter.rs` (the `!matches!(runtime_err, RuntimeError::NotImplemented { .. } | …)`
guard at ~line 5458). So a JVM operand-stack overflow on unverified bytecode (or
a verifier gap) hard-unwound the whole call stack instead of surfacing as a
Java-catchable `java.lang.StackOverflowError` — Java `catch (StackOverflowError)`
/ `catch (Throwable)` could never observe it. This is the wrong-variant mislabel
the report flagged (note `push_checked` already used the catchable
`IllegalStateException`; the plain `push` family used the wrong variant).

## Fix

Changed all **6 overflow sites** in `value_stack.rs` to return the dedicated
unit variant `RuntimeError::StackOverflowError` instead of
`NotImplemented { feature: "operand stack overflow" }`:

- `push` (~315)
- `push_int` (~535)
- `push_long` (~549)
- `push_float` (~563)
- `push_double` (~577)
- `push_null` (~591)

`RuntimeError::StackOverflowError` is a unit variant (`types/src/error.rs:202`)
already mapped to `("java/lang/StackOverflowError", None)` in
`exceptions.rs:393` and routed to a real Java throwable by the interpreter's
runtime-error → exception conversion. So the overflow is now correct **at the
source**, regardless of which interpreter routing arm it hits.

### Why this is the right layer

The interpreter at `interpreter.rs:5503-5523` already carried a *defensive*
string-match workaround that converted
`NotImplemented { feature == "operand stack overflow" }` → `StackOverflowError`
at the slow-path routing point. Fixing the variant at the `ValueStack` source:
- removes the dependency on that brittle string match,
- makes the error correct on **every** path that propagates a `push` `Err`
  (not only the one arm the workaround covers),
- matches the project's existing pattern (`push_checked` etc. already return a
  catchable exception variant).

The interpreter's pre-existing normalization remains harmless (it now simply
never matches the overflow string, because the source no longer emits it).

### Scope discipline / fast path

- The **fast path is unchanged**: only the overflow-branch return value changed;
  the `kinds[]`/`slots[]`/`len` mutation path is byte-identical.
- The `*_unchecked` pushes (verifier-guaranteed) are untouched — they keep their
  `debug_assert!` contract.
- The **underflow** sites (`pop`, `pop_long`, `peek_checked`, …) still return
  `NotImplemented`/`IllegalStateException` — out of scope for B4 (overflow only),
  and underflow is a verifier-impossible internal condition, not a JVMS
  `StackOverflowError`.
- The checked-overflow siblings (`push_checked`, `push_compact_*_checked`,
  `push_with_kind`) already returned the catchable `IllegalStateException` and
  were left untouched.

## Test added

`overflow_returns_stack_overflow_error` (in the existing `#[cfg(test)] mod tests`,
which has `use super::*;` so `RuntimeError` is in scope): asserts each of the 6
overflow sites returns `Err(RuntimeError::StackOverflowError)` specifically (not
just `is_err()`), locking in the catchable-variant contract. Existing overflow
tests (`overflow_fails`, `stack_size_one_overflow`, `push_int_overflow_returns_err`)
only asserted `.is_err()`, so they stay green.

## Behavioral impact

- Java code can now `catch (StackOverflowError)` / `catch (Throwable)` an
  operand-stack overflow instead of the VM hard-aborting the call stack.
- No change to the success path → no perf or correctness impact on verified
  bytecode (the overflow branch is never taken under a sound verifier).

## Files changed

- `vm/src/runtime/value_stack.rs` — 6 overflow returns retargeted to
  `RuntimeError::StackOverflowError` (+ explanatory comment on the shared block);
  one new `#[cfg(test)]` assertion.

## Not done / out of owned scope

- Report Feature-Suggestion #2 ("make `RuntimeError::NotImplemented` itself
  catchable") touches `exceptions.rs`/`interpreter.rs` (not owned) — left alone.
- The interpreter's now-redundant string-match normalization in
  `interpreter.rs:5503-5523` could be simplified, but `interpreter.rs` is not an
  owned file; left in place (harmless — it just never matches now).
