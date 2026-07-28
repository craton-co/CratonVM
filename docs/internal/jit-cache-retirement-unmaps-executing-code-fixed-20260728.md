# JIT cache retirement `munmap`ped code a thread was executing — FIXED (2026-07-28)

Status: ✅ **RESOLVED** for the dominant flavour (~9.7% of stressed runs → 0 in
320). One rarer, differently-shaped fault remains and is tracked separately in
`docs/known-issues/jit-wild-jump-page-aligned-pc-20260728.md`.

## Symptom

Under CPU load, a real-JDK run would die with

```
# SIGSEGV at pc=0x7…375, addr=0x7…375
#  jit pc  : org/springframework/core/ResolvableType.forType(…)
```

`pc == addr` is an *instruction-fetch* fault: the thread was executing at that
address and the page went away. In a core dump the faulting PC sits in a
**hole between `PT_LOAD` segments** — `load35` ends at `0x77a56be2b000`,
`load36` starts at `0x77a56be2e000`, and the PC is `0x77a56be2c375`. Nothing
was mapped there any more.

Two red herrings on the way:

* `CRATONVM_JIT_LEAK_CODE=1` did **not** help. That flag only defers the owners
  routed through `defer_jit_owner`, i.e. inline-cache-held ones — not the
  cache's own retirements.
* The crash handler still *named* the PC (`jit pc : …forType`). That comes from
  the range registry, which had not been cleaned yet; it is not evidence the
  mapping was alive.

## Root cause

`JitCache` publishes each shard copy-on-write: clone the map, insert, then
`ArcSwap::store` the new snapshot. The entry that the new snapshot does **not**
carry forward loses its last strong reference the moment the old snapshot is
released, and `CompiledMethod::drop` `munmap`s the body immediately.

A thread that is merely *executing* a compiled body holds no `Arc` — only a
return address on its stack. So when the **background compiler thread** tier-ups
a method, `put` retires the body the mutator is running inside:

```
[jit-code-free] base=0x77d9a46ab000 len=0x1940 active_jit_executions=2
   1: drop            jit/src/lib.rs:709                     <- munmap
   …
  21: arc_swap::store
  22: put             jit/src/lib.rs:5884
  23: try_jit_compile_callee_slow   interpreter.rs:38552
  25: background_compile_task       interpreter.rs:38904
  28: compiler_loop                 jit/src/tiered.rs:1559
```

`active_jit_executions=2` is the whole bug: the process-wide quiescence counter
said two threads were inside compiled code, and the mapping was released anyway.
Only **5–12** executable buffers are freed in an entire run, so this is a rare,
precisely-timed retirement rather than churn.

## Fix

The quiescence machinery already existed and was already correct —
`defer_jit_owner` parks an `Arc<CompiledMethod>` until `ACTIVE_JIT_EXECUTIONS`
reaches zero, and `jit_execution_leave` drains it. `JitCache` simply never used
it. Every site that drops an artifact out of a shard now routes it through the
new `defer_retired_entries`:

| site | what it retires |
|---|---|
| `JitCache::put` | the entry a tier-up displaces |
| `JitCache::put_osr` | same, for the OSR shard |
| `invalidate_*` (`retain`) | every entry the invalidation removes |
| `clear_all` | every entry in every shard |

Regression test `replaced_body_survives_until_jit_execution_is_quiescent`
(`jit/src/lib.rs`) publishes a body, drops the reader so the shard snapshot is
its only owner, enters an execution epoch, tier-ups the method, and asserts the
old range is still registered — then that it is released after the epoch ends.
Verified to FAIL without the fix ("a tier-up must not release the body a thread
is executing").

## Verification

Stress harness (`docs/internal/repros/resolvabletype-array-receiver-mic-20260728/stress_rtq.sh`):
16 concurrent VMs running `RtEqualsProbe` under 20 CPU hogs, `CRATONVM_JIT_THRESHOLD=1`.

| binary | crashes |
|---|---|
| pristine dev `77389fa06` | **14 / 144** (9.7%) |
| this branch before the fix | 4 / 96 |
| this branch after the fix | **1 / 320** — and that one is the different, still-open shape |

Plus, on the fixed binary: all five probes `MISMATCH_COUNT=0`,
`ConditionalOnPropertyTests` 38/38, cross-module Spring suite `S01..S10`
190/190 at both thresholds, `cargo test -p cratonvm-vm --lib` 2435 pass / 3 fail
(the same 3 that fail on pristine dev).

## Diagnostics added (default-off, kept)

These are what turned an unattributable `SIGSEGV at pc=X` into a two-line
answer, and the next JIT lifetime bug will need them again:

* `ExecutableBuffer` records every unmap into a lock-free ring
  (`recent_code_free_covering`, `code_frees_total`). The crash handler now
  prints `fault pc is inside a RECENTLY FREED code buffer: base=… len=…
  active_jit_executions_at_free=…` — the last field is the one that names the
  bug.
* `CRATONVM_DBG_JIT_CODE_FREE=1` prints a backtrace at every executable-buffer
  unmap. Buffers are freed a handful of times per process, so this is cheap and
  it is the only way to see *which* owner dropped last.
* `CRATONVM_JIT_NEVER_FREE_CODE=1` leaks every retired body, for ANY owner —
  unlike `CRATONVM_JIT_LEAK_CODE=1`, which covers only the inline-cache
  deferral. If a fault vanishes under the former but survives the latter, the
  use-after-free is on a release path the deferral does not reach. That
  distinction is exactly what cost time here.
