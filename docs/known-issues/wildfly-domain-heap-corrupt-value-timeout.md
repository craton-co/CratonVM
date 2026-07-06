# WildFly domain startup timeout with repeated corrupt `Value` cell guard

Status: OPEN (root-cause candidate identified 2026-07-06, needs full-suite confirmation)
Severity: High
First confirmed: 2026-07-05 on Azure worktree `codex/wildfly-nonpassed-probes-20260705-035722`

## Symptom

After fixing the JBoss Modules multi-entry `-mp` bug, `EEConcurrencyExecutorShutdownTestCase` no longer exits immediately during process-controller launch. It now waits the full startup window and fails with:

```text
java.util.concurrent.TimeoutException: Managed servers were not started within [120] seconds
```

The log repeatedly emits the same heap guard diagnostic while the test polls management:

```text
gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) - returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). slot=0x2002600b1d0 raw0="0x0000000100000009" raw1="0x0000000000000000"
```

The management client retries `remote://127.0.0.1:9999` until timeout. The generated domain directory contains configuration and `data/kernel/process-uuid`, but no `process-controller.log` or `host-controller.log` beyond the empty audit log.

## Evidence

Primary run:

```text
/data/wt/wt-wildfly-nonpassed-20260705-035722/apps/wildfly-suite-runner/out/azure-eeconcurrency-mpmulti2-082-jit-real-failed-20260705-160124
```

Key files:

```text
logs/00001-org.jboss.as.test.integration.domain.EEConcurrencyExecutorShutdownTestCase.log
failcauses.log
summary.txt
```

Result summary:

```text
classes: FAIL=1
test-methods: found=1 passed=0 failed=0 errors=1 sum-class-ms=128157
wall-clock=128s
```

## Notes

This is distinct from the fixed process-controller module-path bug. The old immediate `ModuleNotFoundException` and `MDC.put` linkage failure are gone with `cratonvm-wildfly-nonpassed-20260705-035722-mpmulti2`; the remaining failure is a real 120-second domain startup timeout with a repeated guarded heap-corruption signature.

This bug report and the `gen_heap::read_slot` HIB-CV-32 discriminant guard it references were both added in the same commit (`a3728860`, 2026-07-05, "Fix WildFly non-passed suite blockers") — that commit's `gen_heap.rs` changes are unrelated conservative-root-candidate hardening (Family-A), not a fix for this guard's trigger. The root cause of *why* the slot decodes to an out-of-range discriminant (`0x...09` here, valid range is `0..=6`) was left open.

## 2026-07-06 investigation (this session)

Evidence-gathering only — no reproduction achieved, no code change made. Recorded so the next session doesn't re-walk the same ruled-out path.

**Hypothesis 1 (ruled out): plain-field 16-byte `Value` slot tearing.** Same day this doc was filed, three commits closed a real mutator-vs-mutator tearing gap in plain (non-volatile) `getfield`/`putfield`: interpreter `read_slot`/`write_slot` across all three heap backends (`2dfdfddc`), the older JIT `jit_putfield_*` fix (`4e6b560f`), and JIT `jit_getfield`'s read side (`5198fccd`, landed via `investigate/aqs-rwl-writer-contention-jit-hang`) — all now on `dev`. `EEConcurrencyExecutorShutdownTestCase`'s concurrent-executor shape is exactly the kind of code this bug hits, so this looked like the fix at first.

It is **not**, on closer reading of `types/src/value.rs`'s own doc comments (`read_value_atomic`/`write_value_atomic`, `read_value_checked_atomic`): "for a statistically-typed Java field the discriminant word is invariant across stores, so even a cross-word 'torn' pair reconstructs to a valid `Object(ptr-or-null)`/primitive — **never a spliced garbage pointer**." A single Java field only ever stores one `Value` variant, so tearing between two writes to the *same* field can produce a stale or torn *payload*, but never an out-of-range *discriminant* — the exact HIB-CV-32 guard this report's log line trips. Confirmed empirically too: a synthetic repro (two real OS threads, one hammering `h.x = A/B` on a shared `int` field via a JIT-OSR-compiled loop, one reading it, `CRATONVM_DIAG_HIB32=1`) ran ~200M iterations/side with `bad=0` on both a pre-tearing-fix baseline build (`164264c8`) and current `dev` — consistent with this bug class only affecting payload staleness, not discriminant validity. Array element access was also audited (two independent passes) and confirmed **not** to share this bug class at all: array elements are packed native-width primitives or bare 8-byte pointers, never the 16-byte tagged `Value` layout object fields use.

**Hypothesis 2 (best candidate, unconfirmed): JIT `getfield` reference-oop mistagging.** Commit `e60b7a5c` (2026-07-06, `fix/jasper-jdt-parser-aioobe-20260706`, already on `dev`) fixed a *different*, more topically-relevant bug: `getfield`'s three x86-64 codegen paths never marked a reference-typed field's loaded value as a GC oop on the JIT operand stack, so it silently decayed to a plain `Int` (holding the raw pointer bits) at any GC-safepoint or deopt boundary that captured it while uncommitted — confirmed in that investigation via `CRATONVM_DBG_DEOPT=1` showing a `char[][]` field's value captured as `Int(4332917944)` instead of `Object(...)`. This is precisely the shape of "heap reference-integrity defect" HIB-CV-32 exists to survive: a live reference silently mistyped, subsequently mis-relocated/mis-collected/misused, eventually landing on a slot read that decodes bytes belonging to something else (e.g. an object header's `num_slots`/flags fields — `raw0=0x0000000100000009` reads suspiciously like `num_slots=9, some 1-valued flag` rather than any real `Value` encoding). The fix is unconditional (no feature flag), so it applies to any reference-typed `getfield`, not just the JDT parser call sites that surfaced it — plausible for WildFly's heavy dynamic-module/executor code under GC pressure during domain startup.

**Not yet confirmed empirically.** Two synthetic repros were attempted against the pre-fix baseline (`164264c8`, predates `e60b7a5c`): a generic reference-field-getfield-plus-allocation loop, and a closer mirror of the original JDT idiom (`this.intStack[this.intPtr--]` immediately followed by a reference-element-array `System.arraycopy`, inside an OSR-compiled instance-method loop, matching the exact shape documented in `docs/internal/jasper-jdt-parser-arrayindexoutofbounds.md`). Both ran to 200k+ iterations with zero mismatches and no HIB-CV-32 guard hits, confirming OSR-compilation occurred (`CRATONVM_DBG_JITC=1` showed `OSR-compile`/`bg-compile tier=C2`) but not exercising whatever precise interleaving is needed. This matches that same JDT investigation's own account: even the team that root-caused and fixed bug #1 "could not get a minimal standalone Java repro to fail-then-pass" for the *tearing* commit, and needed 12 purpose-built, iteratively-refined synthetic probes (T3–T14) to reliably trigger *this* bug family at all — a repro budget well beyond what this session could allocate as a side-investigation.

**Recommended next step:** re-run `EEConcurrencyExecutorShutdownTestCase` (and ideally the full WildFly domain-mode slice) end-to-end against current `dev` (which now includes `e60b7a5c` and all three tearing fixes). This needs a rebuilt WildFly distribution + Arquillian domain harness on a build host — the prior evidence run's artifacts and the Azure host's WildFly build were both lost to disk-pressure cleanup since 2026-07-05, so this is a from-scratch rebuild (WildFly `install -DskipTests` alone took ~54 min in the original suite run), out of scope for this session. If the guard diagnostic and timeout are gone, close this out referencing `e60b7a5c`; if not, the synthetic repros in `docs/known-issues/repros/wildfly-domain-startup-timeout/` (`FieldTearRepro.java`, `ParserIdiomRepro.java`) are a starting point to iterate into a reliable standalone trigger, same as the JDT investigation's T-series did.

## 2026-07-06 update (parallel session) — boot-infrastructure blocker found; live E2E still not reachable

A second, independent investigation this same day tried the "rebuild the WildFly
distribution and re-run E2E" next step above via a shortcut: rather than a full
`wildfly-core` testsuite + Maven build (not available on the Azure probe host used),
it downloaded a **binary** WildFly 32.0.1.Final distribution from GitHub releases (no
Maven build needed) and drove `bin/standalone.sh`/`bin/domain.sh` directly under a fresh
`dev`-HEAD `cratonvm`, using the same real-JDK/`--nojit` configuration
`apps/wildfly-suite-runner/run-suite.sh` uses.

This did not reach far enough to re-observe (or rule out) either hypothesis above: both
boots stall **before** any application-level service does real, sustained work.
CratonVM only drives the real `Service.start(StartContext)` MSC callback when
`CRATONVM_MSC_REAL_START=1` is set (default off — see
`docs/internal/app-jvm-bugs/handoff-wildfly-msc-service-start.md`, an existing,
separately-tracked, explicitly-incomplete effort). `run-suite.sh` never sets this flag,
so with the default configuration the very first application-level MSC service install
after `WFLYSRV0049 ... starting` never signals completion and the boot hangs
indefinitely — confirmed via `CRATONVM_DEFAULT_WATCHDOG_SEC` + the built-in stack-dump
watchdog to be a genuine parked wait (all non-daemon threads idle in
`EnhancedQueueExecutor$ThreadBody.run`, near-0% CPU), not slow interpretation. Turning
the flag on gets standalone mode further, but into an unrelated
`ServiceNotFoundException` (`BootstrapImpl.internalBootstrap` failing to resolve
`Services.JBOSS_AS`) within ~3 seconds, and makes domain mode hang even earlier with no
error at all. Filed as
[bug-15](../internal/wildfly-suite-bugs/bug-15-msc-real-start-servicenotfound-and-domain-hang.md)
— a boot-infrastructure gap orthogonal to both hypotheses above, but one that must be
resolved (or Maven + the real testsuite restored) before *either* hypothesis can be
confirmed or refuted against a live, sustained-load domain-mode process again.

**Combined recommended next step:** whoever picks this up next needs one of (a) Maven +
a `wildfly-core` testsuite checkout to rerun the actual Arquillian test, or (b) progress
on bug-15 (the MSC real-start gate) so a hand-driven binary-distribution boot can reach
real sustained concurrent execution — only then can Hypothesis 2 above (or a new one) be
tested against a live process again. The `FieldTearRepro.java`/`ParserIdiomRepro.java`
synthetic repros remain the fastest path to iterate on Hypothesis 2 without either.
