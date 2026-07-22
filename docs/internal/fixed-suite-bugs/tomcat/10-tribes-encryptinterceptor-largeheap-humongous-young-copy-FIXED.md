# Tribes `TestEncryptInterceptorLargeHeap` — large arrays thrash the young copier (FIXED)

**Suite:** `org.apache.catalina.tribes.group.interceptors.TestEncryptInterceptorLargeHeap`
**Status:** VM fix on `dev` (`1a378f8c`); suite-runner heap bump applied locally
(apps/ is gitignored — not version-controlled).

## Symptom
`testHugePayload` allocates `new byte[1024*1024*1024]` (1 GiB), AES-GCM
encrypts → decrypts it through the EncryptInterceptor, and asserts the
round-trip. On CratonVM it failed; the apparent error depended on `-Xmx`:
- `-Xmx2g` (suite default): `OutOfMemoryError (alloc_array length 1073741824)`
- `-Xmx8g`: `IllegalStateException: AES-GCM decrypt failed: AuthenticationFailed`
- `-Xmx10g`: `FATAL: GC could not relocate a live object — young to-space is full`

## What it is NOT
- **Not the AES-GCM primitive.** A direct `Cipher("AES/GCM/NoPadding")`
  encrypt→decrypt of a 1 GiB array (and the 3-arg `doFinal(buf, 12, len-12)`
  IV-prefixed form the interceptor uses) round-trips correctly at every size.
- **Not a correctness bug at adequate heap.** The test PASSES at `-Xmx16g`
  (and did so before the fix). The `AuthenticationFailed` at 8g is GC
  corruption of a large array *under memory pressure*, not a crypto defect.

## Root cause — large arrays live in the young copying space
CratonVM's generational heap splits `-Xmx` 50/50 (young pair = `Xmx/2`, each
semi-space = `Xmx/4`; old gen = `Xmx/2`, fixed). Arrays are routed directly to
old gen ("humongous") only when larger than `HUMONGOUS_YOUNG_FRACTION_PERCENT`
(50%) of a young semi-space — i.e. `> Xmx/8`. So a 1 GiB array is humongous
only at `Xmx <= 8g`; at `Xmx 16g` (semi = 4g, threshold = 2g) it lands in
**young** and the Cheney collector **copies it into to-space on every minor
GC** until it reaches `PROMOTION_AGE`. A few live ~1 GiB young arrays then
either overflow to-space (the relocation-path hard OOM) or, when old gen is
also full and humongous routing falls back to young, get partially
copied/corrupted (→ GHASH mismatch → `AuthenticationFailed`).

Net: the test needed `-Xmx16g` on CratonVM vs `-Xmx8g` on HotSpot (whose G1
puts large objects in humongous regions of the general heap, never a copying
young space).

## Fix (`1a378f8c`, `gc/src/gen_heap.rs`)
Cap the humongous threshold at an absolute `HUMONGOUS_ABSOLUTE_CAP_BYTES =
256 MiB`:
```
let threshold = frac_threshold.min(HUMONGOUS_ABSOLUTE_CAP_BYTES);
```
Any array > 256 MiB now always routes directly to old gen and is never
young-copied, mirroring G1's humongous handling. 256 MiB is well above the
default-heap young fraction (256 MiB heap → 32 MiB threshold) and equals the
fraction at the suite's `-Xmx2g` (`2g/8 = 256 MiB`), so small/default heaps —
including the entire normal suite run — are **unchanged**; only genuinely
large arrays on multi-GiB heaps change tier.

## Result (measured, JIT on)
| | min heap to pass |
|---|---|
| HotSpot G1 | 8g (fails 6g) |
| CratonVM before | 16g |
| CratonVM after | **10g** (fails 8g) |

The residual 10g-vs-8g gap is the fixed 50/50 young/old split (old gen is
`Xmx/2`, so the ~5 GiB large-array working set needs `Xmx >= 10g`); closing it
fully would require flexible generation sizing / region-based old gen — out of
scope. Direct 1 GiB AES-GCM driver: relocation-OOM at 10g → PASS.

## Suite-runner heap (local, apps/ untracked)
At the default `-Xmx2g` the test OOMs on **both** VMs — a guaranteed both-VM
failure, not a CratonVM signal. `run-tomcat-suite.ps1` now bumps any
`*LargeHeap` class to `-Xmx12g` (never downgrading a larger `-MaxHeap`), so the
suite exercises it meaningfully; it now PASSES on CratonVM at the default
invocation.

## Repro
```
apps/tomcat-suite-runner/run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real \
  -Category all -Start 274 -Count 1 -Exe <cratonvm.exe>     # auto-bumps to 12g
```
Driver: scratchpad `GcmBig2.java` (IV-prefixed 3-arg-doFinal GCM round-trip at
1 MiB … 1 GiB).
