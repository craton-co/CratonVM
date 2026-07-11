# JIT BCE: missing AIOOBE + silent OOB heap write on multi-array loops — FIXED

**Found:** 2026-07-11, while validating the GPU offload deopt path (the bug is
in the CPU JIT, not the GPU stack). **Severity: HIGH** — silent out-of-bounds
heap stores, i.e. memory corruption plus a missing required Java exception.

**FIXED 2026-07-11** on `fix/jit-bce-per-array-oob-20260711` (`jit/src/x64.rs`
single-pass backend). Root cause was THREE stacked holes, not one — see
"Resolution" at the bottom. Regression tests:
`test_classes/gpu/BoundsDeopt2.java` (single call, OSR),
`test_classes/gpu/BoundsDeopt3.java` (repeated calls, crosses the de-spec
threshold), and `x64::tests::test_bce_*` unit tests over the analysis.

## Repro (minimal, in-tree)

`test_classes/gpu/BoundsDeopt2.java` calls the classic vector-add shape with a
deliberately half-sized output array:

```java
static void vectorAdd(int[] a, int[] b, int[] out) {   // EligibleVectorAdd
    int n = a.length;                                   // n = 2^20
    for (int i = 0; i < n; i++) out[i] = a[i] + b[i];   // out.length = 2^19
}
```

| Run | Result |
|---|---|
| HotSpot JDK 25 | `ArrayIndexOutOfBoundsException` at i = 2^19 ✓ |
| CratonVM `--nojit` | `ArrayIndexOutOfBoundsException` ✓ |
| CratonVM (JIT/OSR, default) | **returns normally — no exception, and i = 2^19 … 2^20-1 stores land beyond `out`'s end** ✗ |

```
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  -cp test_classes/gpu BoundsDeopt2
# NO-EXCEPTION out[1000]=3000 out[last]=1572861     <-- wrong
```

The method is invoked once, so the compiled code presumably comes from OSR of
the hot loop.

## Suspected mechanism

Bounds-check elimination appears to justify eliding the check on `out[i]`
from the loop guard `i < n` where `n = a.length` — a bound that says nothing
about `out.length`. Any loop indexing multiple arrays by the same IV where
only one array's length bounds the loop would be affected. Well-formed inputs
(all arrays same length) never trip it, which is why suites pass; only
mismatched lengths expose it, and then it's a silent OOB write into whatever
object follows `out` in the heap.

## Impact on GPU offload

The GPU kernel's per-access bounds check correctly detects the violation and
deopts to the CPU — but with the JIT on, the CPU re-run can land on the
miscompiled body and still complete silently. With `--nojit` (or a fixed BCE)
the deopt chain produces the correct AIOOBE end-to-end (validated on RTX 2060).

## Where to look

The JIT's BCE pass (README lists "LICM, BCE" for the x86-64 JIT — see
`jit/src/`), specifically how the per-array length domain is established for
stores indexed by the loop IV. A correct elision needs `i < min(bounding
lengths)` per array, or a guard/deopt per distinct array.

## Resolution (2026-07-11)

The suspected mechanism was confirmed — and it was one of THREE independent
holes in `jit/src/x64.rs`, each sufficient to corrupt on its own
(`CRATONVM_JIT_NO_BCE=1` still reproduced, which is how hole 2 was proven
independent of hole 1):

1. **Static BCE elision was not per-array.** `find_safe_array_accesses`
   marked EVERY loop-invariant array indexed by the IV as "statically safe"
   from the single loop test `i < n`, with no proof that `n <= arr.length`
   for each array — and no header guard was emitted for statically-elided
   accesses at all. Fix: static (guard-less) elision now requires
   whole-method **arraylength provenance** (`find_bound_arraylength_provenance`:
   the bound's single dominating store is `aload A; arraylength; istore n`
   and the access's array IS `A`, never reassigned) plus a proven
   non-negative IV start (`find_iv_nonneg_start`). Everything unproven
   demotes to the speculative path, which emits ONE guard PER DISTINCT ARRAY
   (`iv >= 0` once per header, `arr.length >= bound` per array) that deopts
   to the interpreter, which then throws the correct AIOOBE.

2. **The AVX2 element-wise SIMD transform ignored the bounds analysis
   entirely.** `detect_int_array_element_wise` (`out[i] = a[i] OP b[i]` —
   exactly the repro shape) vectorized with zero coupling to
   `bounds_safe_pcs` or the guards, and its batch preheader was emitted
   BEFORE the speculative guards, so even a guarded loop would have
   committed the OOB batch stores before the guard fired. Fix: all three
   SIMD loop transforms (int sum, FP sum, element-wise) are now gated on
   per-array BCE coverage (static provenance or a surviving guard) for
   every array they touch, and the guard block is emitted FIRST in the
   loop-header preheader, before LICM hoists and all SIMD batch code.

3. **Per-bci de-spec dropped guards without restoring the checks they
   justified.** After repeated deopts, the recompile removed the header
   guard but left the guarded PCs in `bounds_safe_pcs` — unguarded elision,
   i.e. the corruption came back on exactly the workload that kept
   deopting. Fix: `SpeculativeBCEGuard::covered_pcs` records which access
   PCs each guard covers; dropping a guard removes them from
   `bounds_safe_pcs` (and, via the coverage gate, disables the SIMD
   transforms that relied on it). `BoundsDeopt3` exercises this: 40
   mismatched calls, every one must throw (pre-fix: 40 silent corruptions).

Verified: `BoundsDeopt2`/`BoundsDeopt3` throw AIOOBE under the default JIT
with heap state identical to HotSpot; the well-formed `Benchmark` vector-add
(16.7M elements) keeps its AVX2 vectorization (confirmed via
`CRATONVM_DBG_JIT_DISASM`) with no measurable regression (guards run once
per loop entry); full `cratonvm-jit` test suite green (897 lib tests + new
`test_bce_*` regression tests).
