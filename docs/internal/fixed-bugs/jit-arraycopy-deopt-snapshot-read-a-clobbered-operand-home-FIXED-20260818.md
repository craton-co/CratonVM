# The `System.arraycopy` intrinsic's deopt snapshot read an operand home the intrinsic itself had already overwritten

**Status: FIXED 2026-08-18** on `fix/compactvalue-ref-provenance-20260818`.
Regression vector: `regression-suite/src/RJitArraycopyRefDeopt.java` (13
checks). Source witness:
`cratonvm_jit::tests::arraycopy_scratch_homes_are_allocated_clear_of_the_operand_homes`.
The original witness was `RMethodSiteCache`, which caught it incidentally.

## The symptom

`RMethodSiteCache` went red on dev with

```
java.lang.NullPointerException
    at RMethodSiteCache.mixedRefAndPrimitive(RMethodSiteCache.java:254)
```

Line 254 is `System.arraycopy(ssrc, 1, sdst, 2, 3)` — a plain, valid
`String[]` copy, immediately after a 20 000-iteration `int[]` copy loop.
Deterministic, JIT-only (`--nojit` passes), and GC-independent (`--Xmx 8g` and
`--XX:UseGc G1` both fail identically).

## The bug

The x64 primitive-copy intrinsic inlines a fast path and branches to an
uncommon-trap deopt stub whenever any guard is unsatisfied — null, non-array,
**reference array**, mismatched element kinds, out-of-bounds. A reference array
is therefore *always* a bail, deliberately, and the interpreter re-runs the
call. That makes the bail's frame snapshot load-bearing for ordinary correct
code, not just for error paths.

The snapshot is taken before the operands are popped, so it names each of the
five by its frame home. The intrinsic then pins all five into scratch homes:

```rust
let s_src     = self.next_spill_offset;
let s_src_pos = self.next_spill_offset + 8;
let s_dst     = self.next_spill_offset + 16;
...
```

`pop_stack` reclaims a Frame slot sitting at the top of the spill region, so
the five pops just above had **rewound `next_spill_offset` back over the very
slots `flush_scratch_registers` spilled the oop operands into**. The scratch
homes alias the operand homes by construction, and the store of `srcPos` to
`s_src_pos` lands on `dst`'s home.

The emitter knew. Its own comment names the hazard exactly — *"storing
s_src_pos (=srcPos) overwrites dst's spilled home BEFORE s_dst reads it"* — and
works around it by loading all five operands into distinct GPRs before storing
any. That fixes the intrinsic's own reads. **It does not reach the second
consumer.** The deopt snapshot still names `dst`'s original home and reads it
when the bail actually fires, by which time it holds `srcPos`.

So the resumed interpreter frame carried an integer, tagged as a reference,
where `dst` belonged. `CompactValue::to_value` refused it (not a plausible heap
pointer), degraded it to null, and `System.arraycopy` threw NPE.

```
x64 frame-deopt entry reason=BoundsCheck at bci=215
  locals=[Undefined, Undefined, Undefined, Object(1768874433160)]
  stack =[Object(1768874432584), Int(1), Object(1), Int(2), Int(3)]
                                          ^^^^^^^^^ dst, as the literal srcPos
```

`locals[3]` is the real `sdst`, sitting right there. The bogus payload **equals
srcPos**, which is the whole proof: changing the call to
`arraycopy(ssrc, 2, sdst, 1, 3)` produces `Object(2)`.

## Why it surfaced now, and why that is not where the bug is

Neither the snapshot (`snapshot_pre_intrinsic_call`) nor the scratch-home block
was touched in the ~99-commit window the regression was attributed to —
`git log -S` over it is empty for both. The defect is older and latent.

What the window landed is `7947017aa`, the RBC.6 lift: OSR may now compile a
method whose bare `athrow` has no local handler. `mixedRefAndPrimitive` throws
an `AssertionError` inside its warm-up loop, so RBC.6 had been refusing it OSR
entry; once lifted, the method compiled and its reference copy reached the
bail. `CRATONVM_JIT_OSR_ATHROW=0` and `CRATONVM_JIT_OSR_DEAD_LOCALS=0` both
restore PASS — not because either is the defect, but because each refuses OSR
for this method.

The bare `athrow` is not required in general: a reduced method with the throw
removed still reproduces. What is required is that the method be compiled with
real deopt frames and that a reference-array `arraycopy` be reached from the
compiled body.

## The fix

Allocate the scratch homes clear of the operand homes, rather than working
around the overlap:

```rust
let mut scratch_base = self.next_spill_offset;
for slot in [src_slot, src_pos_slot, dst_slot, dst_pos_slot, len_slot] {
    if let StackSlot::Frame(off) = slot {
        scratch_base = scratch_base.max(off.saturating_add(8));
    }
}
```

`Frame(off)` is `[rbp - off]` with `off` positive, so pushing the base past
every operand's `off` puts all five scratch homes strictly deeper than every
operand home. That fixes **both** consumers at once — the intrinsic's reads and
the deopt snapshot — which is why it is done here rather than by teaching the
snapshot about the scratch homes. `spill_range_fits` is asked with the new
base, so a frame that cannot afford it bails the compile instead of overflowing.

The GPR pre-load workaround is kept: it costs nothing and pins the ordering
directly.

After the fix the same snapshot reads:

```
stack=[Object(1820681951304), Int(1), Object(1820681951880), Int(2), Int(3)]
                                      ^^^^^^^^^^^^^^^^^^^^ == locals[3]
```

## The first version of the regression vector was a vacuous green

Worth recording, because it nearly shipped. The first `RJitArraycopyRefDeopt`
expressed the same idea more tidily — a counter instead of the throw, the error
cases inline as lambdas, the copies spread over a few more locals — and it
**passed on the broken binary**. Whether the scratch home actually lands on
`dst`'s slot depends on the method's spill layout, so the defect is not
reproducible from the shape alone; that draft asserted the right things about a
layout the defect does not occur in.

The committed `witness()` is therefore kept byte-for-byte in the shape reduced
from the original failure, with the error cases moved to a separate method so
they cannot perturb it. The class comment says so, and says to re-verify RED on
a pre-fix binary after any edit.

## Reproduction (pre-fix)

25 lines, no suite:

```java
public class MinArrayCopy {
    static void mixed() {
        int[] src = new int[16];
        for (int k = 0; k < 16; k++) src[k] = 1000 + k;
        for (int n = 0; n < 20_000; n++) {          // gets the method compiled
            int[] dst = new int[16];
            System.arraycopy(src, 3, dst, 5, 7);
            if (dst[5] != 1003) throw new AssertionError("int arraycopy at n=" + n);
        }
        String[] ssrc = {"a", "b", "c", "d", "e"};
        String[] sdst = new String[5];
        System.arraycopy(ssrc, 1, sdst, 2, 3);      // NPE, pre-fix
        System.out.println("PASS MinArrayCopy");
    }
    public static void main(String[] a) { mixed(); }
}
```

`CRATONVM_DBG=deopt` prints the reconstructed frame directly, which is the
fastest way to see it: look for a stack entry whose payload equals `srcPos`.

## Measured

* `RJitArraycopyRefDeopt`: RED on the pre-fix binary (NPE out of
  `System.arraycopy`), **PASS (13 checks)** after, PASS under `--nojit` on
  both, byte-identical to HotSpot 25.0.3+9.
* `RMethodSiteCache`: PASS, 3/3.
* `regression-suite/run.sh`: see the commit message for the run on this merge.
* The source witness was verified to fail when the scratch homes are put back
  on `next_spill_offset`.

## Related

* `docs/known-issues/` siblings from the same sweep — the RBC.6 lift that
  exposed this is not implicated in it.
* `jit-a-published-synchronized-body-was-served-to-unwrapped-callers-FIXED-20260818.md`
  — the previous day's fix, and the reason this one was found: it was the
  unexplained red left over from validating that one. Same family in one
  respect: a hazard fixed for one consumer and left standing for another.
