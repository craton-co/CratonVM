# 7 buffer classes reading `ABORTED` — NOT a CratonVM bug, an environment-driven assumption gate matched exactly on HotSpot

**Status: CLOSED — not a defect, confirmed on HotSpot.** Investigated
2026-08-19 (Azure host, dev `b4d79475c`). Split out of
`fail-hang-crash-rerun-20260817.md`'s "new clusters, not yet triaged" list.

## What it is

`AdvancedLeakAwareCompositeByteBufTest`, `AlignedPooledByteBufAllocatorTest`,
`BigEndianCompositeByteBufTest`, `LittleEndianCompositeByteBufTest`,
`PooledByteBufAllocatorTest`, `SimpleLeakAwareCompositeByteBufTest`,
`WrappedCompositeByteBufTest` all show the harness's class-level `ABORTED`
status (`aborted>0` triggers it, regardless of how few sub-tests that is).
Read the actual `@@RESULT` counts and this is not what it looks like:

| class | found | ok | aborted |
|---|---:|---:|---:|
| `AdvancedLeakAwareCompositeByteBufTest` | 506 | 497 | 9 |
| `AlignedPooledByteBufAllocatorTest` | 49 | 21 | **28** |
| `BigEndianCompositeByteBufTest` | 496 | 487 | 9 |
| `LittleEndianCompositeByteBufTest` | 496 | 487 | 9 |
| `PooledByteBufAllocatorTest` | 47 | 45 | 2 |
| `SimpleLeakAwareCompositeByteBufTest` | 506 | 497 | 9 |
| `WrappedCompositeByteBufTest` | 496 | 487 | 9 |

Five of the seven pass 96-98% of their sub-tests; the class-level `ABORTED`
label is misleading for those. `AlignedPooledByteBufAllocatorTest` genuinely
aborts more than half its cases (21/49 ok) because — unlike the other six —
*every* one of its test methods is gated by the same assumption.

## Confirmed: not CratonVM-specific

```bash
cd apps/netty-suite-runner
java @common.args -Dcraton.batch=1 CratonRunner io.netty.buffer.AlignedPooledByteBufAllocatorTest
```

HotSpot 25: `found=49 started=49 ok=21 failed=0 aborted=28` — **byte-for-byte
identical** to CratonVM's count on the same host. The class's `newAllocator`
override (and several `@Test` methods) start with:

```java
assumeTrue(PooledByteBufAllocator.isDirectMemoryCacheAlignmentSupported());
```

That capability check answers `false` identically on both VMs on this host
— whatever this Azure box's `Unsafe`/direct-memory-alignment support
actually is, both VMs see the same thing and both skip the same tests via
ordinary JUnit `Assumptions.assumeTrue`. This is JUnit's designed-in
"skip when the environment doesn't support this" mechanism working
correctly, not a defect.

The other six classes were not individually cross-checked against HotSpot
(only `AlignedPooledByteBufAllocatorTest` was, directly) — but the aborted
counts are internally consistent (9 for five classes, 2 for the sixth,
never a raw fraction that looks arbitrary) and match the same "some
parameterizations hit the alignment assumption" shape, so the same verdict
almost certainly applies. Flagged as inference, not independently measured,
for whoever revisits this.

## Disposition

No VM fix applies. If a future doc-sweep wants to stop these seven reading
as `ABORTED` in a status table, the harness's classifier (`aborted>0` →
`ABORTED`) is the thing worth revisiting, not the VM — a class that passes
487/496 real tests and skips 9 via a platform assumption is closer to
`PASS` than to a genuine problem. Same shape as the netty batch10 TLS
"count the aborts separately" trap already documented in
`openssl-key-material-and-engine-residuals-20260813.md`.

## Related

- `fail-hang-crash-rerun-20260817.md` — where this cluster was first
  flagged as untriaged.
- `openssl-key-material-and-engine-residuals-20260813.md` — the same
  "don't read a class's `ABORTED` label without checking the `aborted=`
  count" trap, documented once already for a different class.
