# A panic in a leaf JIT helper still aborts the process

**Status:** OPEN (hardening gap). Found while merging the 2026-09-12 JIT review's
panic-containment work. No reproducer: a helper panic is always a VM bug, and
none is known on these paths.

## Where

`vm/src/jit/helper_guard.rs` contains panics for 49 `extern "C"` helpers. These
16 are deliberately left unguarded:

- the leaf readers `jit_baload`, `jit_iaload`, `jit_aaload`, `jit_arraylength`
  and `jit_getfield`;
- the throw stubs `jit_throw_aioobe`, `jit_throw_arithmetic`,
  `jit_throw_exception` and `jit_npe_with_action`;
- `jit_post_tlab_init`;
- the primitive stores `jit_bastore`, `jit_iastore` and
  `jit_putfield_int` / `_long` / `_float` / `_double`.

A panic in any of them unwinds into an `extern "C"` boundary, which aborts the
process with no Java-visible error.

## Why they are not guarded

A guard has to return *something*, and every answer available at these call
sites is unsound today:

- **A throwable (`Throw`).** Building an `InternalError` allocates. These call
  sites publish no oop map, so a moving collection during that allocation
  could relocate an object the compiled frame still names from an unpublished
  slot.
- **A deopt sentinel (`Deopt`).** The first version of the guard returned
  `i64::MIN` and raised the deopt-pending flag. Nothing proves the sentinel
  resumes at the faulting bytecode for these sites. A resume from an earlier
  point replays side effects the compiled code has already committed, which is
  the silent double execution recorded in
  `jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md`.
- **Dropping the call (`Record`).** For a store, this silently loses a write:
  corruption without a report.

A loud abort is better than any of the three.

## The fix

Give each of these call sites a precise failure exit before guarding it:

1. Record a deopt point, with the frame state at the helper's bci, for every
   leaf helper call. The single-pass tier already records one for the
   implicit-null-check recovery.
2. Have the guard's sentinel branch to that point's stub, so the interpreter
   resumes *at* the helper's bytecode, which has not taken effect.
3. Then guard the helpers with `OnPanic::Deopt`, and let the interpreter raise
   whatever the bytecode really throws.

The stores need the same treatment. The resumed interpreter redoes the store,
so nothing is dropped.
