# `CRATONVM_NO_MOVING_YOUNG=1` disables shadow-stack root publication, and crashes

**Status: 🔴 OPEN.** Found 2026-07-31 while closing
`docs/internal/jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md`.
Not a regression from that work — reproduced on a **pristine `origin/dev`
build** (`9fcd1b63f`).

## The finding

```
CRATONVM_NO_MOVING_YOUNG=1 cratonvm … CratonRunner <ZonedDateTimeTest>
```

SIGSEGVs after ~3 seconds, every run:

```
SIGSEGV at pc=0x…, addr=0x0
  fault pc is inside a LIVE registered code buffer
  slot[r10]: 0x0 …
```

A JIT-compiled body reading a frame slot that holds zero and dereferencing it —
the *reclaimed-root* signature, not a relocation one. The same class runs to
completion on **default flags** (435–447 s, `found=608 ok=404 failed=0`,
two runs), so this is specific to the opt-out.

Reproduced on: `origin/dev` `9fcd1b63f` (clean control),
`fix/jit-c2-gate-residuals-20260731` at three different points, and with
`CRATONVM_JIT_GETFIELD_HELPER=1` (i.e. with that branch's new inline field read
switched off). `OffsetDateTimeTest` behaves identically.

## Mechanism, confirmed by a single-variable experiment

`jit/src/x64/licm.rs`:

```rust
pub fn shadow_stack_maps_enabled() -> bool {
    flags().jit.shadow_stack || (moving_young_enabled() && …)
}
```

So `CRATONVM_NO_MOVING_YOUNG=1` does not only choose a non-moving collector — it
**also turns off the single-pass backend's shadow-stack root publication**,
because that emission was made an implication of moving-young rather than a
property in its own right. The root scan reads the same flag, so the two sides
agree that there is nothing published; what they do not agree with is the
collector's need for those roots.

The experiment that settles it, same binary, same class, one variable:

| lane | result |
|---|---|
| `CRATONVM_NO_MOVING_YOUNG=1` | **SIGSEGV at ~3 s**, every run |
| `CRATONVM_NO_MOVING_YOUNG=1 CRATONVM_SHADOW_STACK=1` | **no SIGSEGV**; still running at the 200 s cut-off |

Putting publication back removes the crash. That does not by itself prove the
non-shadow path is *supposed* to be safe — but it used to be the only path,
before moving-young existed, so "roots are unpublished" should be survivable and
is not. Something else on that path lost the ability to find those roots
conservatively, and that is the actual defect.

## Why this matters beyond the flag

`CRATONVM_NO_MOVING_YOUNG=1` is the comparison lane in several open documents —
it is how the moving-young throughput tax, the `type.temporal` residual and
tomcat 32.1 were all measured. **Any measurement taken in that lane was taken on
a VM whose JIT frames publish no roots**, which is a different program, not a
control. Numbers from it should not be compared against default-flag numbers
without saying so.

It is also the opt-out that the retired moving-young document recommends, and
the one named in `moving_young_disables_optimizing_tier`'s warning text.

## What to do

1. Decide whether shadow-stack publication should be independent of the
   moving-young choice. The doc comment on `shadow_stack_maps_enabled` records
   an A/B showing the emission costs nothing measurable
   (`BinTreesClassic 18`, 6 reps, medians 5640 vs 6045 ms, ranges overlapping),
   so making it unconditional is cheap — but it moves one side of an
   emission/root-scan agreement that must hold exactly, and `gc` and `vm` read
   the same flag. Change all three together or not at all.
2. Or find why the conservative scan no longer covers what the shadow stack
   used to, which is the more valuable answer.

Until then, `CRATONVM_SHADOW_STACK=1` alongside `CRATONVM_NO_MOVING_YOUNG=1` is
a working workaround for anyone who needs that lane.

## Reproduce

```bash
cd /data/data/apps/hibernate-orm-harness/hib-suite-runner
CRATONVM_NO_MOVING_YOUNG=1 cratonvm --java-home <jdk25> @common.args \
  -Dcraton.batch=1 CratonRunner <listfile-with-ZonedDateTimeTest> 0
```

~3 seconds to the fault. Add `CRATONVM_SHADOW_STACK=1` for the clean lane.
