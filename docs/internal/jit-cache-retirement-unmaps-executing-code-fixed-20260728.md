# JIT cache retirement `munmap`ped code a thread was executing (2026-07-28)

Status: ✅ **RESOLVED** by dev commit `3fe14734a`
("fix(jit): retire a superseded artifact only when JIT execution is quiescent"),
which landed **concurrently and independently** while this session was
reproducing the same defect. That commit documented itself only as an addendum
inside an unrelated OSR write-up; this is the standalone record, plus the
quantified before/after, the diagnostics, and the regression test added here.

One rarer, differently-shaped fault survives the fix and is tracked in
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

## Root cause

`JitCache` publishes each shard copy-on-write: clone the map, insert, then
`ArcSwap::store` the new snapshot. The entry the new snapshot does not carry
forward loses its last strong reference right there, and `CompiledMethod::drop`
`munmap`s the body immediately.

A thread that is merely *executing* a compiled body holds no `Arc` — only a
return address on its stack. So when the **background compiler thread** tier-ups
a method, `put` retired the body the mutator was running inside:

```
[jit-code-free] base=0x77d9a46ab000 len=0x1940 active_jit_executions=2
   1: drop            jit/src/lib.rs:709                     <- munmap
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

## Fix (dev `3fe14734a`)

The quiescence machinery already existed and was already correct —
`defer_jit_owner` parks an `Arc<CompiledMethod>` until `ACTIVE_JIT_EXECUTIONS`
reaches zero, and `jit_execution_leave` drains it. `JitCache` simply never used
it. Every site that drops an artifact out of a shard now routes it through
`defer_jit_owner`: `put`, `put_osr`, both invalidation `retain`s, and
`clear_all`.

## Two traps that cost time here

* **`CRATONVM_JIT_LEAK_CODE=1` does NOT mask this.** It only defers the owners
  routed through `defer_jit_owner` — i.e. the inline-cache-held ones — so a
  crash surviving that flag is *not* evidence against a use-after-free. The new
  `CRATONVM_JIT_NEVER_FREE_CODE=1` leaks retired bodies for ANY owner and
  settles the question in one run.
* **The crash handler still NAMES the faulting PC** (`jit pc : …forType`). That
  comes from the range registry, which has not been cleaned at fault time, and a
  *recycled* address would satisfy it too. It is not evidence the mapping was
  alive.

## Verification

Stress harness (`docs/internal/repros/resolvabletype-array-receiver-mic-20260728/stress_rtq.sh`):
16 concurrent VMs running `RtEqualsProbe` under 20 CPU hogs, `CRATONVM_JIT_THRESHOLD=1`.

| binary | crashes |
|---|---|
| pristine dev `77389fa06` (pre-fix) | **14 / 144** (9.7%) |
| this branch before the fix | 4 / 96 |
| with the fix | **1 / 320** — and that one is the different, still-open shape |

Regression test `replaced_body_survives_until_jit_execution_is_quiescent`
(`jit/src/lib.rs`, added here — the dev commit shipped no test) publishes a
body, drops the reader so the shard snapshot is its only owner, enters an
execution epoch, tier-ups the method, and asserts the old range is still
registered — then that it is released after the epoch ends. Verified to FAIL
without the fix ("a tier-up must not release the body a thread is executing").

## Diagnostics added here (default-off, kept)

These are what turned an unattributable `SIGSEGV at pc=X` into a two-line
answer, and the next JIT lifetime bug will need them again:

* `ExecutableBuffer` records every unmap into a lock-free ring
  (`recent_code_free_covering`, `code_frees_total`). The crash handler now
  prints `fault pc is inside a RECENTLY FREED code buffer: base=… len=…
  active_jit_executions_at_free=…` — that last field is the one that names the
  bug, and it needs no flag to have been set in advance.
* `CRATONVM_DBG_JIT_CODE_FREE=1` prints a backtrace at every executable-buffer
  unmap. Buffers are freed a handful of times per process, so this is cheap and
  it is the only way to see *which* owner dropped last. It produced the trace
  quoted above.
* `CRATONVM_JIT_NEVER_FREE_CODE=1` — see the traps section.

These complement dev's `CRATONVM_DBG_JIT_UNMAP=1`, which names each buffer as it
is unmapped but must be enabled before the run and correlated by hand.
