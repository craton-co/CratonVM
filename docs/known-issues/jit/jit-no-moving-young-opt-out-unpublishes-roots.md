# `CRATONVM_NO_MOVING_YOUNG=1` crashes — root publication was keyed on the collector

**Status: 🟡 PARTIALLY FIXED 2026-07-31.** The first of two independent faults
in this lane is fixed; the second is isolated to a named mechanism and stays
open below. Supersedes
[`repros/no-moving-young-lever-crashes-20260731.md`](../repros/no-moving-young-lever-crashes-20260731.md),
which found the same lane broken from a different workload the same day.

## Why the lane matters

`CRATONVM_NO_MOVING_YOUNG=1` is the standard A/B lever for anything
moving-young-related — it is named in `moving_young_disables_optimizing_tier`'s
own warning text and it produced the cost tables in the retired
[moving-young gate doc](../../internal/jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md)
and in tomcat 32.1. While it crashes, none of those tables can be re-derived,
and any new measurement that reaches for it reads as an unrelated failure.

## Fault 1 — publication withdrawn with the collector (FIXED)

`shadow_stack_maps_enabled()` (`jit/src/x64/licm.rs`) and its mandatory twin
`conservative_roots::shadow_stack_enabled()` (`vm/src/jit/`) both read

```rust
flags().jit.shadow_stack || (moving_young_enabled() && …)
```

The shadow stack is how a JIT frame's live references become visible to the
collector at all. Keying it on `moving_young_enabled()` made **root visibility
a property of which young collector runs**, so the opt-out silently withdrew
it. On a pristine `origin/dev` build (`9fcd1b63f`), Hibernate
`ZonedDateTimeTest` and `OffsetDateTimeTest` then SIGSEGV'd in **1–3 s**,
`addr=0x0`, fault pc inside a live JIT code buffer, `slot[r10]: 0x0` — the
reclaimed-root signature. The same classes **pass on default flags**
(447/435/391 s and 367/308 s, `failed=0` every run).

Single-variable proof: `CRATONVM_NO_MOVING_YOUNG=1 CRATONVM_SHADOW_STACK=1`
— publication restored, nothing else changed — ran clean past the 200 s
cut-off, 2/2.

**Fix:** the `moving_young_enabled() &&` term is removed on **both** sides in
one change, leaving `CRATONVM_JIT_MY_SHADOW_EMISSION=0` as the joint opt-out.
The default path is byte-identical (moving-young is on by default, so the
expression already evaluated true). Pinned by
`x64::tests::shadow_stack_maps_enabled_does_not_depend_on_the_young_collector`,
which additionally asserts publication survives a moving-young override.

### Also tried, and reverted

Making the safepoint scratch-register flush unconditional
(`emit_pre_safepoint_spill_impl`, currently `moving_young_enabled() &&
scratch_flush_at_safepoint_enabled()`). It is a plausible second
root-visibility hole — it spills operand-stack oops living in caller-saved
scratch registers, which the callee-saved blind spill never covers — and it
made things **worse**: the `NO_MOVING_YOUNG + SHADOW_STACK` lane went from
clean to a deterministic SIGILL, 2/2. Calling it there reserves spill slots and
rewrites `self.stack` at a point the non-precise frame layout did not budget
for. Recorded at the call site so the next person does not re-try it blind.

## Fault 2 — a one-byte-off control transfer into the safepoint spill run (OPEN)

With publication restored, the lane still dies — now `SIGILL`, deterministically
(3/3), and **this is not caused by the fix**: the same failure reproduces on
`cratonvm-m5`, a build of this branch's merge base *before* either change, in
the `NO_MOVING_YOUNG + SHADOW_STACK=1` configuration (2/2).

gdb, at the fault:

```
=> 0x…6ec:  (bad)
0x…6cc: 48 89 9d b0 fe ff ff   MOV [rbp-0x150], RBX
0x…6d4: 48 89 b5 a8 fe ff ff   MOV [rbp-0x158], RSI
0x…6dc: 48 89 bd a0 fe ff ff   MOV [rbp-0x160], RDI
0x…6e4: 4c 89 85 98 fe ff ff   MOV [rbp-0x168], R8
0x…6eb: 4c 89 8d 90 fe ff ff   MOV [rbp-0x170], R9      <-- instruction starts here
```

The fault pc is `0x…6ec` — **one byte inside** the 7-byte spill at `0x…6eb`.
Nothing falls through into the middle of an instruction, so control was
*transferred* there: some computed address is the true boundary **+1**. The run
itself is the full-GPR blind spill (`ALL_SPILL_GPRS`) that
`emit_pre_safepoint_spill_impl` emits at every GC-capable safepoint.

Isolation, same binary, same class:

| configuration | result |
|---|---|
| `NO_MOVING_YOUNG=1` (publication fixed) | **SIGILL** 3/3 |
| `NO_MOVING_YOUNG=1 CRATONVM_NO_PRECISE_REG_SPILL=1` | **clean** past 200 s |
| `NO_MOVING_YOUNG=1 CRATONVM_NO_PRECISE_JIT_MAPS=1` | **clean** past 200 s |
| default flags | clean, always |

So the fault needs the safepoint register-spill run **and** the non-moving
lane. Both lanes emit the run; what differs is that `emit_post_safepoint_reload`
is emitted only when `precise_maps && !moving_young_enabled()`, and that the
scratch flush before the run is emitted only under moving-young — i.e. the two
lanes' spill runs sit at different offsets relative to whatever recorded the
address.

It is also **newer than the lane's other fault**: the same configuration was
clean on this branch's pre-merge build (`cratonvm-b3`, 2/2), and broke with the
merge of `origin/dev` — the window that carries the raw JIT-to-JIT
shadow-stack/mirror work (`83a55dd61`, `cac4cbac0`), which inserts and moves
instructions around calls and epilogues.

### Where to start

Find what can transfer control INTO the spill run: a recorded
`native_pc_offset` / `DeoptimizationPoint::native_offset`, an OSR entry, or an
exception-handler resume. One of them is computed one byte past an instruction
boundary in this configuration. `CRATONVM_NO_PRECISE_REG_SPILL=1` is a working
workaround for anyone who needs the lane meanwhile — as is
`CRATONVM_NO_PRECISE_JIT_MAPS=1`, which is broader.

## Reproduce

```bash
cd /data/data/apps/hibernate-orm-harness/hib-suite-runner
CRATONVM_NO_MOVING_YOUNG=1 cratonvm --java-home <jdk25> @common.args \
  -Dcraton.batch=1 CratonRunner <listfile-with-ZonedDateTimeTest> 0
```

Seconds to the fault. The Windows-side repro in the superseded document
(`DateSymbolsProbe`, `apps/tomcat-suite-runner/probes/`) is ten seconds and
needs no harness.
