# ES CRASH — `GenerationalHeap::get_field` stack overflow — FIXED (was `Path.toString()` infinite recursion)

Status: FIXED — 2026-07-19/20, `native-builtins/src/phases_late.rs`

## Symptom

`EXCEPTION_STACK_OVERFLOW` (`0xC00000FD`) very early in process startup
(within ~1s of the last clinit-fixup log line, before any real test method
runs). The crashing PC symbolized to
`cratonvm_gc::gen_heap::GenerationalHeap::get_field`/`read_slot`
(`gc/src/gen_heap.rs`) across different runs — but that was a red herring:
`get_field` isn't buggy, it's simply the next function called once the real
stack was already exhausted by unrelated recursion elsewhere. The raw
native frame walker is deliberately skipped for
`EXCEPTION_STACK_OVERFLOW` (walking an exhausted stack would itself fault
— `vm/src/runtime/crash_handler.rs`), so the crashing PC alone gave no call
chain.

## Root cause

`native-builtins/src/phases_late.rs`'s `p57_read_path` (the internal
helper behind `java.nio.file.Path`'s native method registrations,
including `toString()`) has a fast/slow path:

- **Fast path**: read the path string directly from field 0 (CratonVM's
  synthetic `Path` layout).
- **Slow-path fallback** (for real-JDK `Path` implementations with a
  different layout, e.g. delegating wrapper Paths): `ctx.invoke_virtual(
  path_obj, "toString", "()Ljava/lang/String;", &[])` — a genuine virtual
  dispatch, on the theory that it would land on either a real delegating
  wrapper's own bytecode `toString()`, or (before an unrelated same-day fix)
  the historically-dead-dispatched `Object.toString()`.

Three **independently-correct**, same-day (2026-07-19) fixes combined into
a regression:

1. `docs/internal/springboot/path-tostring-dead-dispatch-breaks-inprocess-javac-FIXED.md`
   — added a force-native gate so `Path.toString()` dispatch stops dead-ending
   at `Object.toString()`.
2. `docs/internal/springboot/path-tostring-indy-stringconcat-dead-dispatch-FIXED.md`
   — made `vm_exec.rs`'s `invoke_on_class_shared_inner` **receiver-aware**:
   ANY object whose runtime class `is_subclass_of` `Path` now dispatches
   `toString()` straight to the registered native (`p57_path_display_string`
   → `p57_read_path`), regardless of the call site's compile-time symbolic
   type (fixing `String.valueOf(Object)`/indy-string-concat call sites where
   the receiver happens to be a `Path`).
3. This doc's bug: `p57_read_path`'s slow-path fallback (above) now
   redirects, via fix #2, straight back into `p57_read_path` itself — for a
   `Path` object whose fast-path field-0 read keeps failing, NOTHING about
   the object changes between calls, so the fallback recurses into itself
   **forever**. Confirmed empirically: added a stack-overflow crash hook
   (`vm/src/runtime/crash_handler.rs` now dumps the `dispatch_trace` ring on
   `EXCEPTION_STACK_OVERFLOW`, since the native call stack itself is
   unwalkable) and reproduced with `CRATONVM_DBG_LETSGO=1` — the full
   256-slot ring was **one single entry, repeated 256/256 times with zero
   variation**: `TestRuleTemporaryFilesCleanup.initializeJavaTempDir() ->
   NATIVE Path.toString()`.

This recursion is invisible to all of CratonVM's counted stack-depth guards
(the Java-frame counter, the `EXEC_DEPTH` re-entrant-native guard, and the
JIT-dispatch depth ceiling) since none of them count `ctx.invoke_virtual`
calls made from native Rust code — it recurses purely on the real OS
thread stack with no soft limit, producing a hard, uncatchable
`EXCEPTION_STACK_OVERFLOW` instead of a catchable
`java.lang.StackOverflowError`.

## Fix

Added a thread-local re-entrancy guard around `p57_read_path`'s fallback
dispatch (`native-builtins/src/phases_late.rs`): the first call takes the
real dispatch as before (the common, legitimate delegating-wrapper case
terminates immediately, since it dispatches into different, real bytecode);
a nested re-entry into this exact fallback — which can only happen via the
recursive-redirect case above — returns the same benign empty-string
fallback the function's other failure arm already uses, instead of
recursing again.

## Verification

- Original crashing repro (`ES812PostingsFormatTests#testDocsAndFreqsAndPositionsAndPayloads`,
  seed `B17AC9D3E1F2A0C4`, default config): was a 100% reproducible
  `EXCEPTION_STACK_OVERFLOW` within ~1s (3/3 pre-fix attempts — ban-lifted,
  ban-restored, and a byte-for-byte clean `origin/dev` build, confirming it
  was unrelated to the separate `LUCENE-POSTINGS.1` JIT-ban investigation
  it was discovered during). Post-fix: `OK (1 test)`, 137.559s, zero crash.
- Full 32-test `ES812PostingsFormatTests` class (random seeds, not the
  fixed corrupting seed): 17/19 completed cleanly before the suite's own
  580s timeout on the heaviest remaining fuzz method — a performance limit
  (unrelated, pre-existing), not a crash. Zero `AIOOBE`/`SIGSEGV`/`corrupt`/
  `overflow`/`fatal error` across the whole run.
- `cargo test --release -p cratonvm-native-builtins --lib`: 3040 passed, 0
  failed.

Not yet committed/merged as of writing — see the containing worktree's
session state.

## Note for future crash investigations

The `dispatch_trace` ring-buffer dump-on-`EXCEPTION_STACK_OVERFLOW` hook
added to `vm/src/runtime/crash_handler.rs` during this investigation is a
permanent, general-purpose diagnostic improvement, not a one-off: any
future stack-overflow crash can be diagnosed the same way — rerun with
`CRATONVM_DBG_LETSGO=1` and look for the same repeated-entry-with-zero-variation
signature in the `dispatch_trace dump (label=stack-overflow...)` section of
stderr.
