---
name: spring-boot-buildsrc-coldpath-hangs-2026-06-22
description: Historical Spring Boot buildSrc JUnit suite re-run on dev d95a836e (2026-06-22), P-core-pinned + watchdog-off + 360s. Archived 2026-07-01: the HotSpot-passable CratonVM-only hangs from this index are fixed or tracked in newer focused docs; ProjectBuilder rows are environment-limited and not primary CratonVM bugs.
metadata:
  type: internal
  area: jit, gc, throughput, groovy, gradle
---

# Spring Boot buildSrc suite — cold-path true-hangs (dev `d95a836e`, 2026-06-22)

> **ARCHIVED 2026-07-01:** This file is a historical index for the 2026-06-22
> buildSrc coldpath run. The two HotSpot-passable CratonVM-only hangs called out
> here are no longer active: `PluginXmlParserTests` was fixed by SBR-02, and
> `SpringRepositoriesExtensionTests` is now recorded as **11/11 FULL GREEN** in
> the newer Groovy/indy Mockito handoff and was re-verified on current `dev`
> with `CRATONVM_JIT_ALLOW_PACKAGES=groovyjarjarantlr4/`. The remaining Gradle
> `ProjectBuilder` rows are environment-limited; broader ANTLR cold-path
> throughput and latent deep-recursion stack recovery stay tracked in
> [`jit-deep-recursion-fault-recovery.md`](../../known-issues/jit-deep-recursion-fault-recovery.md).
> Archived SpringRepos handoff:
> [`springrepos-extension-hang-jit-throughput-and-deep-recursion.md`](../../springrepos-extension-hang-jit-throughput-and-deep-recursion.md).

Fresh full re-run of the `apps/spring-boot/buildSrc` JUnit suite (the only compiled
test module in that checkout) under an optimized fat-LTO `cvsbtest` binary built
from `dev` `d95a836e`, vs HotSpot 25. Harness `buildSrc/runner/compare-suite.sh`.

## Headline numbers
- **23 non-TestKit classes** run (5 TestKit excluded — fork Gradle daemons).
- **17 match HotSpot**, **0 crashes**, **0 wrong-result / extra-fail**.
- **5 true-hangs** + **1 slow-pass** (initially mis-reported as 6 hangs).
- Functional battery (`cratonvm-suite`, 10 scenarios / 95 checks): **10/10 == HotSpot**.

## Methodology note (why the first count was wrong)
The first sweep ran **unpinned on a hybrid P+E-core machine, watchdog on, 240s/class**.
Reclassification **pinned to P-cores (`0xFFFF`), default watchdog disabled, 360s** moved
`AntoraAsciidocAttributesTests` from "HANG" to **SLOW-PASS (21/21 in 269s)** — it had
merely exceeded the 240s timeout. The 17 passing classes are correct but **3–110×
slower** than HotSpot (e.g. `DependencyVersionUpgradeTests` 63/63 in 111s vs 1s). This
is the **cold-path interpreter throughput tax**, the same family as
the Hibernate HQL reproducer tracked in
[`jit-deep-recursion-fault-recovery.md`](../../known-issues/jit-deep-recursion-fault-recovery.md).

## The 5 true-hangs (P-core-pinned, watchdog off, killed at 360s)

| Class | HotSpot | In known-issues before? | Notes |
|-------|---------|-------------------------|-------|
| `mavenplugin.PluginXmlParserTests` | PASS 2/2, 1s | **✅ FIXED** | **RESOLVED** — was a `java.util.regex` / `String.replaceAll`+literal-`replace` throughput wall in `PluginXmlParser.format()`. Fixed by flipping `CRATONVM_NATIVE_STRING_REGEX` default-ON (SBR-02, `0d7dfc28`, merge `01375f90`): the regex/replace chain routes to fast Rust-regex natives. Mirror probe `RegexLoopProbe` 100k iters nojit 7s / JIT 7.4s, output byte-identical to HotSpot (was 300s+ hang). Writeup: [`SBR-02-string-regex-throughput.md`](SBR-02-string-regex-throughput.md). |
| `groovyscripts.SpringRepositoriesExtensionTests` | PASS 11/11, 5s | Historical | **RESOLVED by later work and re-verified on current dev.** The 2026-06-22 run contradicted the older springrepos note, but the subsequent multi-layer investigation is fixed through the Groovy/indy Mockito tail; [`spring-boot-groovy-indy-mockito-mock-dispatch.md`](../spring/spring-boot-groovy-indy-mockito-mock-dispatch.md) records **11/11 FULL GREEN**. A 2026-07-01 retry also passed 11/11 with the guarded ANTLR lift enabled. Archived handoff: [`../spring/springrepos-extension-hang-jit-throughput-and-deep-recursion.md`](../../springrepos-extension-hang-jit-throughput-and-deep-recursion.md). Residual infrastructure tracker: [`../../known-issues/jit-deep-recursion-fault-recovery.md`](../../known-issues/jit-deep-recursion-fault-recovery.md). |
| `antora.GenerateAntoraPlaybookTests` | 1/2 — **env-fail**, 3s | n/a | **NOT a clean bug — env-limited** (see below) |
| `artifacts.ArtifactReleaseTests` | 7/8 — **env-fail**, 3s | n/a | **NOT a clean bug — env-limited** |
| `autoconfigure.DocumentAutoConfigurationClassesTests` | 1/2 — **env-fail**, 3s | n/a | **NOT a clean bug — env-limited** |

### The 3 Gradle-`ProjectBuilder` classes are environment-limited (NOT counted as CratonVM bugs)
HotSpot **fails the same test in each** with
`org.gradle.api.GradleException: Could not inject synthetic classes` at
`org.gradle.testfixtures.internal.ProjectBuilderImpl.getGlobalServices` →
`ProjectBuilder.build()`. `ProjectBuilder` requires Gradle's **instrumented test
runtime** (a bytecode-instrumentation agent), which a standalone `java -cp … RunJUnit`
launch does not provide — so **neither VM can pass these here**; the "correct env" is a
real Gradle `test` task, not wired in this curated checkout (no root `settings.gradle`).
Per the comparison contract (behaviour matching HotSpot is not a bug), these are
**excluded from the bug count**. The only CratonVM-specific residue is that CV **hangs**
where HotSpot throws the `GradleException` fast (CV stalls in Gradle
`messaging/actor/ActorFactory.defineClass` + the cross-thread JIT-root WARN instead of
surfacing the same exception) — a secondary robustness divergence on a test that cannot
pass on either VM, **not a primary functional bug**.

**Net active CV-only hang count on HotSpot-passable classes: 0.**
`PluginXmlParserTests` is fixed by SBR-02, and `SpringRepositoriesExtensionTests`
is fixed by the later SpringRepos/Groovy-indy workstream. The cold ANTLR
throughput follow-up remains real but is tracked by the deep-recursion/fault-recovery issue, not
by this historical suite index.

## Shared stall signature
The 4 non-Antora-bootstrap hangs all stop logging on the VM's own warning, then go
silent until the 360s kill:

```
WARN cratonvm_vm::jit::conservative_roots: scan_active_jit_frames: another thread holds
live JIT frames while this thread's JIT chain is empty ... the cross-thread STW JIT root
scan is a tracked follow-up ... cross_thread_jit_gap_hits=1 global_jit_depth=6
```

This is the **Family A / A4 cross-thread STW JIT-root gap** documented in
[[fork6-fjp-multithread-jit-root-reclamation]] and the README's 2026-06-22 dev refresh
("A4 is OPEN… the cross-thread STW JIT-root gap is still *exercised*… the tracked
follow-up"). Whether it is the **cause** of these stalls or an incidental WARN over a
throughput stall is not yet separated — both the JIT-root gap and the cold-path
interpreter throughput are in play. No crash signature (no panic / SIGSEGV / heap
corruption) appears in any hang log.

## Triage
- **✅ FIXED:** `PluginXmlParserTests` — SBR-02 native String regex default-ON (see table).
- **✅ FIXED by later work:** `SpringRepositoriesExtensionTests` — the follow-up
  Groovy/indy Mockito cascade is documented as 11/11 green; this row is no longer
  a known-issues entry.
- **HANDOFF / lower-priority** (HotSpot also env-fails; CV divergence is hang-vs-terminate):
  the 3 Gradle `ProjectBuilder` classes.

## Config matrix (`--nojit` + `-XX:+UseG1GC`, 17 passing classes, P-core-pinned)
- **`--nojit` (interpreter-only): 17/17 PASS** — zero interpreter-path divergences.
- **`-XX:+UseG1GC`: 8 of 17 "hang" @240s, but all SLOW-PASS correctly** given a 600s
  window (e.g. `DependencyVersionTests` 8s-serial → 366s-G1 PASS 8/8). Not a deadlock.
  **UPDATE 2026-06-22 — root-caused + FIXED** (not merely generic G1 maturation tax):
  the dominant cost was G1's `is_addr_in_live_region` taking `regions.lock()` + an
  O(num_regions) linear scan **per word** of the conservative JIT/native root scan
  (run on every object-returning native call), a hot path `gen_heap` had already made
  lock-free O(1). After the port (`fix/g1-coldpath-hang`, merged to dev),
  `DependencyVersionUpgradeTests` G1+JIT went **1264s → 72s** and all `bom.bomr`
  classes pass under G1+JIT. Full detail (moved out of known-issues):
  [`../app-jvm-bugs/spring-boot-g1-conservative-rootscan-region-lookup-FIXED.md`](../app-jvm-bugs/spring-boot-g1-conservative-rootscan-region-lookup-FIXED.md).
  A *non-pathological* G1-vs-serial gap remains under the G1-maturation workstream.

## Net genuine CratonVM-only defects from this run
1. ~~`PluginXmlParserTests` true-hang~~ — **✅ FIXED** (SBR-02 native String regex default-ON; writeup [`SBR-02-string-regex-throughput.md`](SBR-02-string-regex-throughput.md)).
2. ~~`SpringRepositoriesExtensionTests` true-hang~~ - fixed by later SpringRepos/Groovy-indy work and re-verified on current dev; see [`spring-boot-groovy-indy-mockito-mock-dispatch.md`](../spring/spring-boot-groovy-indy-mockito-mock-dispatch.md) and archived handoff [`../spring/springrepos-extension-hang-jit-throughput-and-deep-recursion.md`](../../springrepos-extension-hang-jit-throughput-and-deep-recursion.md). Residual deep-recursion infrastructure work lives in [`../../known-issues/jit-deep-recursion-fault-recovery.md`](../../known-issues/jit-deep-recursion-fault-recovery.md).
Everything else is env-limited (ProjectBuilder ×3), a slow-pass (Antora, G1 ×8), or clean.

## Reproduce
```bash
CV="C:/craton/CratonVM-sbtest/target/release/cvsbtest.exe"; JDK="C:/Program Files/Java/jdk-25"
cd apps/spring-boot/buildSrc; CP="runner;$(cat test-classpath.txt)"
# pin to P-cores (Windows): start "" /affinity FFFF ...  (verify children inherit 0xFFFF)
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 360 "$CV" --java-home "$JDK" \
  --stack-dump-on-timeout 0 -cp "$CP" RunJUnit <FQCN>
```
> Note: `--stack-dump-on-timeout N` is the watchdog deadline **in seconds** (0 = off),
> NOT a boolean — passing `1` arms a 1-second abort.

Full per-run evidence: `apps/spring-boot/cratonvm-bug-reports/SB-RUN-2026-06-22-*`.
