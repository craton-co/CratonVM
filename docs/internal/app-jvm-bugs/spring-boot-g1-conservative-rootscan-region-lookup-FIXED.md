# Spring Boot buildSrc — `-XX:+UseG1GC` JIT-on throughput cliff (FIXED)

**Status:** FIXED on branch `fix/g1-coldpath-hang` (`gc/src/g1.rs`).
**Found:** 2026-06-22 (filed as the `spring-boot-g1-gc-coldpath-hang` known issue,
dev `d95a836e`). **Root-caused + fixed:** 2026-06-22 (dev `df11ac00`).

## What was filed
Several Spring Boot `buildSrc` JUnit classes that pass under the default (serial)
GC and under `--nojit` were reported to **HANG** (killed at the 240 s harness
timeout, empty/short logs) when the only changed flag was `-XX:+UseG1GC`. The
filed doc left an explicit open question: **true G1 deadlock, or a throughput
slow-pass past the 240 s timeout?**

## Resolution of the open question — it was never a deadlock
The decisive 600 s, P-core-pinned, uncontended re-runs settled it:

| Class | serial+JIT | `--nojit` | G1+nojit | G1+JIT (before) | G1+JIT (after fix) |
|-------|:----------:|:---------:|:--------:|:---------------:|:------------------:|
| `ArtifactVersionDependencyVersionTests` | 11 s | ✓ | ✓ | **59 s** | **14 s** |
| `InteractiveUpgradeResolverTests` | (62 s, contended) | ✓ | ✓ | 8 s | 11 s |
| `CalendarVersionDependencyVersionTests` | 4 s | ✓ | ✓ | 6 s | 5 s |
| `DependencyVersionUpgradeTests` *(not in the original matrix)* | 57 s | 8 s | 9 s | **1264 s** | **72 s** |

Findings:

1. **The three originally-flagged classes were a contention-amplified slow-pass,
   not a defect.** They complete under G1 in 6–59 s when run uncontended/pinned.
   The default-G1 code is byte-identical between `d95a836e` and `df11ac00` (the
   only intervening `g1.rs` commits are the opt-in `CRATONVM_G1_PARALLEL_EVAC`
   parallel-evac fixes, default-off). The original "HANG" was the documented G1
   throughput tax pushed past 240 s by the compare-suite harness's *deliberate*
   concurrent-session CPU contention (it explicitly does not kill stray
   `cratonvm` peers).

2. **A genuine G1+JIT throughput cliff exists** — surfaced by the heavier
   `DependencyVersionUpgradeTests` (63 tests): serial+JIT 57 s, G1+nojit 9 s, but
   **G1+JIT 1264 s** (≈22×). A `--stack-dump-on-timeout` capture showed a single
   thread (`main`) actively executing bytecode deep in JUnit
   `AnnotationUtils.findAnnotation` / `findMetaAnnotation` recursion (via
   `TimeoutExtension.beforeEach`) — **no blocked threads, no lock wait → not a
   deadlock**. With `--verbose:gc`, **zero** young/mixed collections fired (the
   256 MB heap never filled), so it was not a GC-pause or GC-thrash problem
   either. It was finite (the 1264 s run completed 63/63), just catastrophically
   slow — and only under the **G1 + JIT** combination.

## Root cause
`G1Collector::is_addr_in_live_region` is the per-word predicate of the
**conservative JIT/native root scan** (`conservative_roots::scan_active_jit_frames`,
folded into `interpreter::update_root_snapshot`), which runs on *every
object-returning native call*. When a long-lived compiled JIT frame sits near the
stack bottom (here the compiled JUnit `withInterceptedStreams` lambda that drives
the whole test plan), the conservative scan band spans the **entire** deep
interpreter recursion above it, so the predicate is invoked across megabytes of
stack words per native call (this is the documented "JIT-on slower than the
interpreter" mechanism the WS1 JIT-scan cache was added to mitigate).

The G1 implementation did, **per candidate word**:

```rust
let regions = self.regions.lock();          // take the regions mutex …
for r in regions.iter() { …linear scan all ~256 regions… }
```

i.e. a `parking_lot` mutex acquire **plus an O(num_regions) linear scan** for
every word examined — including the overwhelming majority that are plainly
non-heap (return addresses, ints, native-stack addresses). `gen_heap`
(serial GC) had **already fixed this exact pattern**: its `is_object_address`
comment notes "the old triple-mutex check … ran per operand-stack object on every
object-returning native call and contended catastrophically" and replaced it with
a **lock-free cached `[base, end)` bounds** test. **G1 never received the port** —
hence serial+JIT is fast and G1+JIT falls off a cliff.

## The fix (`gc/src/g1.rs`)
Port the lock-free bounds gate to G1, exploiting the fact that the G1 backing
arena is a single contiguous `Box<[u8]>` allocated once and never moved/resized:

1. Cache immutable `arena_base` / `arena_end` fields (set in `G1Collector::new`).
2. `is_addr_in_live_region` first does an **O(1) lock-free** reject of any address
   outside `[arena_base, arena_end)` — this short-circuits the vast majority of
   conservative-scan words with no lock and no scan.
3. For the rare in-arena candidate, index the **single** owning region in O(1)
   (`idx = (addr − arena_base) / region_size`; equivalent to the existing
   `lookup_region_for_addr` binary search but O(1)) instead of linearly scanning
   all regions. `HumongousContinuation` slices are treated as live (their
   `cursor` is a `0` sentinel; the `HumongousStart` region carries the full-span
   cursor), preserving the previous scan's semantics for humongous interiors.

Behaviour is identical to the old linear scan for every real object address; the
only difference is harmless conservative over-retention of any tail padding past
a humongous object's true end (already tolerated by conservative root scanning,
and still rejected precisely by `is_object_address`'s header check).

## Validation
- `DependencyVersionUpgradeTests` G1+JIT: **1264 s → 72 s** (≈17.6×), 63/63 pass.
- All 10 `bom.bomr` buildSrc classes pass under G1+JIT (ArtifactVersion 59 s→14 s,
  ReleaseTrain 29 s→12 s, others ≤11 s).
- `binarytrees 16` at `-Xmx64m` (forces real G1 evacuations through the changed
  root-scan path) == HotSpot `14985902` under both G1+JIT and G1+nojit.
- `cargo test -p cratonvm-gc`: **771 pass / 0 fail** single-threaded (the one
  intermittent failure under the parallel runner —
  `gen_heap::tests::non_moving_sweep_when_unregistered_jit_frame_on_stack`, an
  untouched conservative-stack-scan test — is pre-existing parallel-execution
  flakiness; passes deterministically in isolation).

## Residual / not in scope
A *non-pathological* G1-vs-serial throughput gap remains (e.g. ArtifactVersion
14 s G1 vs 11 s serial) — that is the broader G1-maturation throughput item, not
this cliff. The catastrophic per-word lock+linear-scan cost is gone.

## Reproduce
```bash
CV=C:/craton/CratonVM-g1hang/target/release/cvg1fix.exe; JDK="C:/Program Files/Java/jdk-25"
cd apps/spring-boot/buildSrc; CP="runner;$(cat test-classpath.txt)"
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" --java-home "$JDK" -XX:+UseG1GC \
  --stack-dump-on-timeout 0 -cp "$CP" RunJUnit \
  org.springframework.boot.build.bom.bomr.version.DependencyVersionUpgradeTests
# before fix: ~1264 s; after fix: ~72 s; both 63/63 pass.
```
