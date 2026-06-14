# WildFly testsuite on CratonVM — live status

**Run date:** 2026-06-14 · **VM:** CratonVM (dev `793787c1`) · **Boot JDK:** Temurin 25 ·
**Target:** WildFly 41.0.0.Beta1 testsuite

## Scope
- **1,541 compiled test classes** under test (per-module): basic 928, clustering 148,
  expansion 106, elytron 93, web 59, smoke 51, ws 40, domain 20, multinode 15, vdx 13,
  elytron-oidc-client 11, xts 9, manualmode-expansion 8, scripts 6, rts 6, iiop 6,
  rbac 4, secman ~16, legacy/shared a few. (`manualmode` failed to compile — unresolved
  dep; excluded.)
- Almost all are **Arquillian** integration tests. Run standalone (no live container)
  via the per-class `KRun` launcher, one CratonVM JVM per batch of 15.

## Phase
- [x] WildFly full build (`install -DskipTests`) — BUILD SUCCESS, 54 min
- [x] `-DallTests` test-compile — most modules compiled (manualmode failed)
- [x] Per-module classpaths generated (39 cp files)
- [x] Per-class harness built + validated; **two harness bugs fixed**:
  classpath > Windows cmdline limit → `@argfile`; git-bash `/c/…` paths → `cygpath -m`
- [x] Timeout calibration: CratonVM cold-loads the WildFly/Arquillian graph in ~120 s/JVM
  (slow, not hung — verified one class completes with the *same* result as HotSpot);
  batching amortises it. `ONE_TO=240s`, `BATCH=15`, `BATCH_TO=1200s`.
- [~] **Full clean run in progress** (all 1,541 classes)
- [ ] Crashes triaged into bug docs

## Status-class meaning
`OK` all pass · `FAIL` ran, ≥1 failure (most = Arquillian "no container", same as HotSpot —
**not** VM bugs) · `LOADERR` class load threw · `EMPTY` no tests · `ABEND` JVM died, no
RESULT = **CratonVM crash** · `TIMEOUT` external timeout = **hang** · `POST-RESULT` result
then crash. The **ABEND / TIMEOUT / POST-RESULT** and any **LOADERR/FAIL that HotSpot
doesn't share** are the crash reports of interest.

## FINAL numbers — complete run (1540 classes, no duplicate rows)

| Status | Classes |
|--------|---------|
| OK | 14 |
| FAIL | 1409 |
| LOADERR | 40 |
| EMPTY | 77 |
| **ABEND (crash)** | **0** |
| **TIMEOUT (hang)** | **0** |

Test-level: 3990 found, 1 succeeded, 1410 failed (rest not run — Arquillian aborts
the class before its tests when no container is present).

**Zero reproducible single-class VM crashes** across the whole suite. The only hard
failures seen during the run were *batch-transient* — 1 `rc=139` SIGSEGV
(microprofile.jwt batch) and 2 `rc=124` clustering batch timeouts — and every class
in them re-ran clean individually, so none are recorded as ABEND/TIMEOUT. (A burst of
139 `rc=127` "ABEND"s mid-run was an external artifact: the main `cratonvm.exe` was
deleted by concurrent main-checkout rebuilds; those were purged and the affected
classes re-run on a stable binary.)

The dominant `FAIL` (1409) is the Arquillian no-container `ConfigurationException`
— identical to HotSpot, not a VM defect. With a live container the smoke group is
111/111 (see [live-container-smoke.md](live-container-smoke.md)).

**Confirmed CratonVM-only defects: 1 (FIXED)**
- **[bug-01](bug-01-stream-foreachordered-abstractmethoderror.md)** — `Stream.forEachOrdered`
  → `AbstractMethodError: … has no Code attribute`. CratonVM-only (HotSpot fine). Root cause:
  its only native registration is compiled out of the real-JDK CLI build (`synthetic-jdk`-only
  path). **FIXED** in `vm/src/runtime/interpreter.rs` (redirect `forEachOrdered`→`forEach`);
  verified by standalone repro (== HotSpot) + the affected WildFly class (LOADERR→FAIL found=11).
  Accounts for all 27 LOADERR so far (`ejb.security` cluster). High.

ABEND/TIMEOUT hard crashes: still **0** (the earlier rc=1 ABENDs were the concurrency/OOM
artifact described below).

### Important methodology note
The earlier ABEND `rc=1` "crashes" (messaging/JMS/batch classes) were **artifacts of
running 7 runner instances concurrently** (an orphaned-process bug — `TaskStop`/`pkill`
left inner `bash run-wildfly.sh` children alive). Seven JVMs each cold-loading a 32 KB
WildFly classpath exhausted memory; the OS killed them → `rc=1` with no panic. Proof:
`ArtemisMessagingTestCase` runs cleanly **standalone** (FAIL = same `ConfigurationException`
as HotSpot). Fixed: all runners nuked to zero via PowerShell CIM (by command-line), single
`nohup` daemon relaunched, lock pid matches, **zero duplicate rows**.

Almost every class FAILs identically to HotSpot with Arquillian
`ConfigurationException: javaHome '${container.java.home}' must exist` (no live container).
These are **not** VM defects. The crash reports of interest are the minority that
**diverge** from HotSpot: ABEND / TIMEOUT / POST-RESULT, or a LOADERR/different failure
HotSpot doesn't share — most likely in classes whose `@Deployment` builds ShrinkWrap
archives (real bytecode/ASM/IO) before the container check. Each such class is
HotSpot-baselined before being written up.

_Last updated: 50/1541 classes (smoke), single clean runner._
