---
name: spring-boot-g1-gc-coldpath-hang
description: CratonVM G1 throughput finding (2026-06-22, dev d95a836e), NOT a deadlock. Spring Boot buildSrc JUnit classes that pass fast under serial GC + --nojit "hang" (exceed 240s) under -XX:+UseG1GC — but a 600s-watchdog-off recheck shows they SLOW-PASS correctly (DependencyVersionTests 8s-serial -> 366s G1 PASS 8/8; SingleRowTests 7s -> 317s PASS 7/7). G1 here is ~40-46x slower than serial, beyond the documented 10-20x. Correctness is fine; this is the G1-maturation throughput gap. 8 of 17 classes affected. P-core-pinned.
metadata:
  type: known-issue
  area: gc, g1, throughput
---

> **RESOLVED — moved out of `docs/known-issues/` 2026-06-22.** This was the
> as-filed finding. A follow-up investigation showed the slowness was **not**
> merely the generic G1-maturation throughput tax this doc concluded with: the
> dominant per-class cost was a **specific, fixable defect** — G1's
> `is_addr_in_live_region` took `regions.lock()` + an O(num_regions) linear scan
> for **every word** of the conservative JIT/native root scan (run on every
> object-returning native call), a hot path `gen_heap` had already made lock-free
> O(1) but G1 never had ported. Fixed on branch `fix/g1-coldpath-hang`
> (`gc/src/g1.rs`): `DependencyVersionUpgradeTests` G1+JIT **1264s → 72s**, all 10
> `bom.bomr` classes pass under G1+JIT, `binarytrees 16` == HotSpot.
>
> **Full root-cause + fix writeup:**
> [`spring-boot-g1-conservative-rootscan-region-lookup-FIXED.md`](spring-boot-g1-conservative-rootscan-region-lookup-FIXED.md).
>
> Two clarifications to the as-filed numbers below: (1) the "deadlock vs
> slow-pass" question is settled — **slow-pass, never a deadlock** (all classes
> complete; single thread executing bytecode, no blocked threads). (2) The ~45×
> figures here were measured on `cvsbtest`@`d95a836e` and were further inflated by
> the compare-suite harness's deliberate concurrent-session contention; run
> uncontended on the fixed binary these classes complete in seconds (the genuine
> residual cliff was the heaviest class, `DependencyVersionUpgradeTests`, now
> fixed). A *non-pathological* G1-vs-serial gap remains and stays tracked under
> [[project_g1_maturation]].

# Spring Boot buildSrc — `-XX:+UseG1GC` is ~40–46× slower than serial (slow-pass, NOT a deadlock)

**Found:** 2026-06-22, dev `d95a836e`, optimized `cvsbtest`, **P-core-pinned (`0xFFFF`)**.
Harness: `apps/spring-boot/buildSrc/runner/run-configs.sh` (config matrix `cmp-configs/`)
+ `run-g1recheck.sh` (`cmp-g1recheck/`).

## Verdict: correctness OK, throughput is the gap
Classes that pass fast under the default **serial** GC (and under `--nojit`) **appear to
hang** (exceed the 240s harness timeout) under `-XX:+UseG1GC`. A controlled recheck —
**600s timeout, watchdog off, P-core-pinned** — proves they actually **SLOW-PASS with
correct results**:

| Class | serial GC | `--nojit` | `-XX:+UseG1GC` (240s run) | G1 recheck (600s) |
|-------|:---------:|:---------:|:------------------------:|:-----------------:|
| `bom.bomr.version.DependencyVersionTests` | PASS 8/8 (8s) | PASS | "HANG" @240s | **PASS 8/8 in 366s** |
| `context.properties.SingleRowTests` | PASS 7/7 (7s) | PASS | "HANG" @240s | **PASS 7/7 in 317s** |

8s → 366s is **~46×**; 7s → 317s is **~45×**. So this is **not a deadlock and not a
correctness bug** — it is extreme G1 throughput tax (beyond the documented ~10–20×
serial:G1 ratio in [[project_g1_maturation]] / `docs/feature-designs/concurrent-gc-maturation.md`).

## Scope (config matrix, 240s/class, P-core-pinned)
Under `-XX:+UseG1GC`, **8 of 17** classes exceed 240s; the **9** that finish are all the
tiny/fast classes (≤4 tests, little allocation). The 8 that don't (by serial time):

`InteractiveUpgradeResolverTests` (62s), `ArtifactVersionDependencyVersionTests` (18s),
`ConfigurationPropertiesAnalyzerTests` (18s), `ReleaseTrainDependencyVersionTests` (14s),
`CalendarVersionDependencyVersionTests` (9s), `DependencyVersionTests` (8s),
`SingleRowTests` (7s), `DependencyVersionUpgradeTests` (111s).

By the ~45× factor, all of these would slow-pass given a large enough window
(`DependencyVersionUpgrade` at 111s serial would need ~80+ min). **`--nojit` passes all
17** — the throughput cliff is specific to the G1 collector path, not the interpreter.

## Characterization
- **No crash / panic / `inconsistent header`** in any G1 log — pure slowness.
- `LibraryTests` and the other small classes pass under G1, so **G1 init + collection
  work**; the cost scales with allocation/GC-cycle count.
- Not a new correctness defect; it belongs to the **G1 maturation throughput** workstream
  (serial pauses already ~10–20× HotSpot; the buildSrc churn pushes the gap to ~45×).

## Reproduce
```bash
CV="C:/craton/CratonVM-sbtest/target/release/cvsbtest.exe"; JDK="C:/Program Files/Java/jdk-25"
cd apps/spring-boot/buildSrc; CP="runner;$(cat test-classpath.txt)"
# pin to P-cores; give it a real window — it PASSES, just slowly:
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 600 "$CV" --java-home "$JDK" -XX:+UseG1GC \
  --stack-dump-on-timeout 0 -cp "$CP" RunJUnit \
  org.springframework.boot.build.bom.bomr.version.DependencyVersionTests   # PASS 8/8 ~366s
```
Logs: `apps/spring-boot/buildSrc/runner/cmp-configs/g1/`, `runner/cmp-g1recheck/`.
