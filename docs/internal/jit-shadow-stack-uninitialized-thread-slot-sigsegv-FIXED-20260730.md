# JIT test-binary SIGSEGV cluster: uninitialized shadow-stack thread slot (FIXED 2026-07-30)

Status: fixed.

## Symptom

On Windows, `cargo test --release -p cratonvm-jit --lib -- --test-threads=1`
crashed with `STATUS_ACCESS_VIOLATION` (0xc0000005) partway through the
suite. Skipping past the crashing test with `--skip <name>` reliably
produced a *different* crashing test further along
(`x64::tests::cooperative_poll_runs_in_a_pure_compiled_method`,
`x64::tests::live_monitor_ops_execute_direct_runtime_stubs`,
`x64::tests::test_aconst_null`, `x64::tests::test_compile_dconst` were all
observed) — the full suite had never actually run to completion on this
platform.

Each individual test passed in isolation (`--exact`, one process per test),
which initially suggested test-order-dependent state corruption. It also
appeared build/worktree-dependent: a byte-identical source commit crashed
reliably in one worktree (even after a full `cargo clean --release` and
rebuild) while an independently checked-out fresh worktree of the exact same
commit passed cleanly. Both observations turned out to be symptoms of the
same underlying cause — reads of uninitialized stack memory — not separate
bugs; see "Why it looked build/order-dependent" below.

## Root cause

`jit/src/x64.rs`'s shadow-stack precise-root mechanism (default-on whenever
`moving_young`/`shadow_stack_maps_enabled()` is on, which is the current
default — see `types/src/flags.rs::DEFAULT_MOVING_YOUNG`) caches the
current `*mut JvmThread` in a per-invocation frame slot
(`shadow_thread_slot_off`) so safepoint push/reload sites can reach the
shadow stack's `top` without a helper call per safepoint.

The **prologue**'s cache-fetch block (`Compiler::emit_prologue`) is gated on
three conditions:

```rust
if self.shadow_enabled
    && self.helpers.get_current_thread != 0   // <- helper must be wired
    && self.shadow_thread_slot_off != 0
{
    // zero the slot, then (unless later NOP'd) call get_current_thread()
    // and store the result
}
```

But three other sites that *read* the same slot only checked two of those
three conditions — they were missing the `helpers.get_current_thread != 0`
check:

- `Compiler::emit_shadow_push` (emitted around GC-capable call sites)
- `Compiler::emit_shadow_reload` (emitted immediately after such calls)
- `Compiler::emit_epilogue`'s "restore the `top` watermark" block

`jit/src/x64.rs`'s own unit-test harness (`test_helpers()`) intentionally
leaves `get_current_thread` null — it's a stub table for tests that don't
need real thread-context integration. In that context, the prologue's block
is skipped *entirely* (correctly, per its guard) — but the epilogue (and,
for methods with GC-capable calls, the push/reload sites) still ran, because
their guards didn't check the same condition. They loaded
`shadow_thread_slot_off` — a stack slot the prologue never initialized — as
if it held a validated `*mut JvmThread`, and when that uninitialized garbage
happened to be non-null, dereferenced it: `mov [garbage_ptr + offset],
some_other_garbage`. On Windows this is a wild-pointer write, hence
`STATUS_ACCESS_VIOLATION`.

Confirmed by direct instrumentation (temporary `eprintln!` in a local debug
build) on the crashing `test_aconst_null` case:

```
get_current_thread=0x0 shadow_thread_slot_off=32 shadow_savetop_slot_off=24
```

— the slot is allocated (`32`, i.e. `[rbp-32]`) but never written, and the
epilogue's restore block ran anyway and crashed reading/writing through it.

### Why it looked build/order-dependent

This is textbook undefined-behavior sensitivity, not a second bug: the
*value* of uninitialized stack memory is whatever a prior function call in
that thread's history happened to leave there. That depends on the exact
call chain and stack-frame layout of everything that ran before the
compiled test method — which can differ between:

- **Test order / accumulated process state** — different `--skip`-adjusted
  runs exercise a different sequence of prior JIT compiles and Rust calls
  before reaching the crashing test, leaving different garbage in the same
  physical stack slot.
- **Different worktree builds of "the same" commit** — absolute source
  paths baked into debug info differ by worktree path length, which can
  shift unrelated local frame layouts enough to change what garbage ends up
  in this slot, without changing program logic.
- **Isolated single-test runs** — a fresh process's thread has a much
  shorter, more consistent call history before reaching the JIT invocation,
  so the slot more often coincidentally reads as zero (the null-guard then
  skips safely, "passing" by luck rather than by correctness).

The real `cratonvm` VM binary never hit this: `vm/src/.../build_helpers()`
always wires `get_current_thread` whenever precise maps are on, so the
prologue's fetch always runs and the slot is always properly initialized
before any read site — the asymmetric guard never mattered there. This is
purely a JIT-unit-test-harness-triggered bug (`test_helpers()`'s
intentionally-stubbed `get_current_thread`), not a defect reachable through
real bytecode execution.

## Fix

Added the matching `self.helpers.get_current_thread != 0` guard to all
three read sites (`emit_shadow_push`, `emit_shadow_reload`,
`emit_epilogue`), so they are gated identically to the prologue's write
site. In production (`get_current_thread` always wired) this is a no-op —
byte-identical codegen. In the JIT unit-test harness, the shadow push /
reload / epilogue-restore blocks now consistently skip (matching what the
prologue already does), instead of reading uninitialized memory.

## Validation

- `cargo test --release -p cratonvm-jit --lib -- --test-threads=1`: was
  crashing (`STATUS_ACCESS_VIOLATION`) partway through; now **1053 passed,
  0 failed, 0 crashed** — the full suite runs to completion for the first
  time on this platform. Verified in a from-scratch, independently checked
  out worktree (not just the worktree the bug was found in).
- `cargo test --release -p cratonvm-gc --lib -- --test-threads=1`: 873
  passed, 0 failed (unaffected, included as a broader sanity check since
  `gc` and `jit` share GC-root-tracking assumptions).
- The four previously-known crashing tests
  (`cooperative_poll_runs_in_a_pure_compiled_method`,
  `live_monitor_ops_execute_direct_runtime_stubs`, `test_aconst_null`,
  `test_compile_dconst`) all pass individually and as part of the full
  suite.
