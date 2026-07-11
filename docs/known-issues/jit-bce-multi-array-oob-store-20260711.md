# JIT BCE: missing AIOOBE + silent OOB heap write on multi-array loops

**Found:** 2026-07-11, while validating the GPU offload deopt path (the bug is
in the CPU JIT, not the GPU stack). **Severity: HIGH** — silent out-of-bounds
heap stores, i.e. memory corruption plus a missing required Java exception.

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
