# JIT `newarray` heap exhaustion: SIGSEGV → catchable `OutOfMemoryError` (FIXED)

**Status:** FIXED. Branch `feat/oom-throw` (worktree `C:\craton\CratonVM-ircall`),
merged to `dev`.

## Symptom

CratonVM crashed with `EXCEPTION_ACCESS_VIOLATION` (SIGSEGV, `0xC0000005`)
instead of throwing `java.lang.OutOfMemoryError` when the Java heap was too
small for an allocation-heavy JIT-compiled workload.

Repro (`scratch/ircall/`):

- `IrCallGc.java` at `--Xmx 32m` crashed natively; at `--Xmx 64m` it completed
  (`total=2721637800000`). HotSpot runs fine at `-Xmx32m`.
- `BigArrayOom.java` (a hot/JIT-compiled `int[] alloc(int n){…}` then
  `alloc(100_000_000)` in `try/catch(OutOfMemoryError)`) crashed deterministically
  at any heap < the request.

The crash was backend-independent — it lived in the JIT allocation path on heap
exhaustion, not in any single optimizer.

## Root cause

Two layered defects in the fallible JIT `newarray` path:

1. **Null-deref on OOM (the SIGSEGV).** `jit_newarray` (vm/src/jit/helpers.rs)
   returns the `0`/null sentinel on heap exhaustion, but the single-pass
   `newarray` (0xbc) codegen pushed RAX straight onto the operand stack with no
   null-check. The very next `arraylength` / `getfield` / array-store
   dereferenced `[null + offset]` → SIGSEGV before the method could return.

2. **Thread-TLS not set on the fast path (the silent-wrong-answer, exposed once
   the SIGSEGV was guarded).** The fallible OOM helper `jit_alloc_oom` needs the
   per-thread `JIT_THREAD` TLS — both to run the allocation-failure STW GC
   (`maybe_gc_forced`, helpers.rs:1201) and to construct the `OutOfMemoryError`
   object (helpers.rs:1265). A method whose only "interesting" op is a `newarray`
   (e.g. `int[] alloc(int n){ return new int[n].length; }`) has no
   invoke / direct_call / bounds-check / null-store-stub / athrow, so
   `has_dispatch` was **false** → the interpreter's fast compiled-entry path
   skipped `set_jit_thread`. With a null thread, `jit_newarray` skipped GC and
   `jit_alloc_oom` couldn't create the OOME, so the bail returned `i64::MIN` with
   **no pending exception**. The interpreter then read `i64::MIN` as the method's
   `int` return value — and `i64::MIN as i32 == 0` — so `new int[N]` silently
   yielded `0` instead of throwing (`BigArrayOom` printed `NO OOM (unexpected)`).
   In `IrCallGc`'s nested `consume`, the same `0`-instead-of-throw under-counted
   the total at small heaps (this had been mis-diagnosed as a separate
   single-pass "register-root" GC bug — it was not; the interpreter-only path was
   always correct).

## Fix (minimal: `jit/src/x64.rs` + `vm/src/jit/helpers.rs` only)

1. **`emit_post_alloc_oom_check()`** after the `newarray` (0xbc) helper returns:
   `TEST RAX,RAX; JZ → shared exception-check stub`. The shared stub loads the
   `i64::MIN` deopt sentinel and runs the epilogue (reused from the invoke
   exception guard). This removes the null deref (the SIGSEGV).

2. **Force `has_dispatch` for any method that emits that bail** — new compiler
   flag `emitted_alloc_oom_check`, OR'd into the `has_dispatch` computation
   exactly like the existing `direct_calls` / `athrow` thread-availability rules
   (x64.rs ~21617). Now an allocating method takes the dispatch-aware entry that
   calls `set_jit_thread`, so `jit_newarray` can GC and `jit_alloc_oom` can build
   the OOME. The OOME is then drained + routed through the method's own exception
   table by the **existing** general-exception drain on the has_dispatch return
   path (interpreter.rs ~19049) — giving a JIT'd `newarray` the same catchable-OOM
   semantics as the interpreter's `gc_alloc_array`.

`jit_newarray` was already converted to the fallible `try_alloc_array` →
`jit_alloc_oom` shape; `jit_alloc_oom` is the shared OOM signal (stash OOME in
`JIT_PENDING_EXCEPTION`, return `0`).

**Not in scope / not wired:** `jit_new_object` / `jit_anewarray_object` still use
the non-fallible `alloc_object` / `alloc_array` (which fall back to the old
generation before a hard abort). Converting *those* to catchable OOM needs a
fallible-with-old-gen path — a separate follow-up.

### Why no interpreter.rs change was needed

An earlier iteration added a universal "part C" general-exception drain on every
JIT return path. It proved redundant: every code path that can set a pending
general exception (dispatch helpers, `athrow`, and now the newarray OOM bail)
already forces `has_dispatch`, and the general-exception drain is deliberately
placed in the has_dispatch branch (interpreter.rs:19049). Forcing `has_dispatch`
(fix #2) routes the OOM through that existing drain, so the universal drain was
reverted — keeping the fix to two files and respecting the existing design.

## Validation (all == HotSpot)

- `BigArrayOom` @ 64m and @ 32m → `caught OutOfMemoryError s=1600000` (was
  SIGSEGV, then silent `0`). `[DBG_OOM]` (temporary) confirmed `thread_set=true
  exc_set=true` after fix #2.
- `IrCallGc` @ 32m (primary repro) → `total=2721637800000` (no crash, correct).
  Also correct under `CRATONVM_DBG_GC_STRESS=1`.
- `IrCallGcCatch` @ 32m and @ 48m → `completed total=272016378000000`.
- Regression: `bintrees10/14/18` = 135854 / 3222190 / **68332206**;
  `IrCall.java` (inc-22 IR-call probe) gate off+on = 23762906400000;
  `sieve250k` (newarray-heavy) checksum 22044. `cratonvm-jit` lib tests
  804/6/11 pass.

### Blast radius

Only methods that emit a primitive `newarray` (0xbc) become `has_dispatch`
(object `new` 0xbb and `anewarray` 0xbd are untouched). Those methods need the
thread for GC anyway, so the dispatch entry is correct, not just safe. bt
(object-`new`-heavy, no primitive newarray in its hot loop) keeps the fast path —
bt18 golden unchanged.

## Pre-existing, unrelated

`cargo test -p cratonvm-jit --test intrinsic_arrays_ops` has a pre-existing red
test `test_arrays_fill_null_array_deopts` (verified identical failure on the
unmodified dev tree). Tracked separately; not caused by this fix.
