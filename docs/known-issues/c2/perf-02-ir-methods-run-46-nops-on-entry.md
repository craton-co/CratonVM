# PERF-02 — every IR method runs 46 NOPs on entry; `fib` pays it 2.27e9 times

**Status: FIXED 2026-08-05**, same day it was found. Closeout, with the
disassembly, the A/B and the residual it leaves behind:
`../../internal/perf-02-ir-thread-fetch-nop-sled-FIXED-20260805.md`.

**Found 2026-08-05. Introduced 2026-07-31 (`545c99add`).**
**Owns:** `IrLowerer::finish_lazy_thread_fetch` in `../../../jit/src/ir_lower.rs`.
Not `../../../bench`, not the tier thresholds, not the shadow stack's
existence.

## The finding in one diff

Both backends emit the shadow-stack `get_current_thread` fetch in the prologue,
then erase it in place when the method turns out to publish nothing. The
single-pass backend erases it with a `JMP` over the span. The IR backend only
NOP-filled it, so the ~46-byte span stayed straight-line executable code on the
entry path of every such method:

```
29: mov [fs:0FFFFE080h],rbp
32: 90 nop      <-- 46 of these, no jump over them
5f: 90 nop
60: mov r11,<safepoint flag>
```

`CratonBench fib` is a two-line static method entered 2.27e9 times. It went
4,240 ms -> 8,400 ms, **1.96x**.

## Why the levers all said "not me"

`CRATONVM_JIT_MY_SHADOW_EMISSION=0`, `CRATONVM_NO_MOVING_YOUNG=1`,
`CRATONVM_JIT_MY_SELFCALL_PROOF=0`, `CRATONVM_TIER_C2_THRESHOLD` and
`CRATONVM_TIER_C2_MIN_INVOCATIONS` at 2e9 all produce a **byte-identical** body:
176 instructions, 46 NOPs, every time. This is unconditional codegen. An
inert lever is not an elimination, and here every lever was inert.

What found it was `CRATONVM_DBG_JIT_DISASM=CratonBench.fib` — a direct
good-vs-bad codegen diff that needs no build, after a `git bisect` on the shared
host had been killed twice by the OOM reaper.

## Fixed by

One shared `ExecutableBuffer::erase_range_with_jump_over`, called by both
backends so the halves cannot drift apart again, plus a `debug_assert!` for the
invariant the jump depends on (nothing may branch into the span).

Pinned by a **byte assertion**, not by behaviour: this defect cannot fail a
functional test, because a run of NOPs is perfectly correct — it just executes.
`erase_range_with_jump_over_emits_a_jump_not_a_nop_sled` asserts the `0xEB`
opcode and the exact displacement.

## Verified

1.74x on `fib` against its own parent commit (8 pairs interleaved, user CPU
time, ranges disjoint), and all seven phase checksums identical.

## What it leaves open

A **1.32x residual** against the 2026-07-23 single-pass body, which is the other
per-call instrumentation the IR body carries and the single-pass body did not —
the prologue slot zeroings, the frame record on entry *and after every call
return*, the per-call bytecode-index store, and the epilogue savetop-restore on
all four exits. **Every one is provably dead under the same
`!shadow_pushed_any` condition that erases the fetch**, so the next increment is
to record those spans and erase them the same way. Deliberately not bundled in,
so the 1.74x attributes to one change.
