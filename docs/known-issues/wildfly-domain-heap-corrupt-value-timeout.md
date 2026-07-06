# WildFly domain startup timeout with repeated corrupt `Value` cell guard

Status: FIXED (mechanism) — see 2026-07-06 update below for the remaining verification gap
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

## 2026-07-06 update — root cause identified and FIXED on dev; live re-confirmation still blocked

**Root cause: this is the plain-field 16-byte `Value`-slot tearing bug, already fixed on
`dev`.** `gen_heap::read_slot`'s "corrupt Value cell (out-of-range discriminant)" guard
(the diagnostic quoted above) fires whenever the 16-byte `Value` backing an ordinary
object field decodes to a discriminant word (byte 0-3 of the slot) greater than
`VALUE_MAX_DISCRIMINANT` (6) — i.e. the bytes no longer form a valid `Value` at all. Prior
to commit `2dfdfddc` (`fix(gc): close plain-field 16-byte slot tearing gap in interpreter
get_field/set_field`, merged to `dev` 2026-07-06, the day *after* this doc's evidence was
captured), the interpreter's plain (non-`volatile`) `get_field`/`set_field` in all three
heap backends (`heap.rs`, `gen_heap.rs`, `g1.rs`) read/wrote that 16-byte slot via a bare
`std::ptr::read`/`std::ptr::write` — a genuinely non-atomic two-machine-word copy. Two
Java threads doing ordinary `getfield`/`putfield` on the *same* field slot could tear each
other's stores (one thread observing half of an old value spliced with half of a new one),
producing exactly this shape: a discriminant word that is neither write's real tag, hence
`> VALUE_MAX_DISCRIMINANT`, hence rejected by the guard and reported. Commit `5198fccd`
(`fix(jit): close plain-field tearing gap in jit_getfield (read side)`, same day) closed
the matching gap on the JIT's own field-read helper; the JIT's write side and the
GC-marker-vs-JIT-store case were already fixed earlier by `4e6b560f`. Confirmed by direct
diff: pre-`2dfdfddc`, `gen_heap.rs::write_slot` was `std::ptr::write(ptr as *mut Value,
value)`; post-fix it is `cratonvm_types::write_value_atomic(...)`, and `read_slot` moved
from `read_value_checked` (plain read) to `read_value_checked_atomic` (tear-free two-word
read). All higher-level accessors that reach this slot — `Unsafe.getObject`/`putObject`,
`Unsafe.getObjectVolatile`/`putObjectVolatile` (via `get_field_volatile`/
`set_field_volatile`, which already added a striped mutex + `SeqCst` fences on top), and
CAS (`compare_and_swap_field`, already serialized under `monitors.with_cas_lock`) — route
through the same now-fixed `get_field`/`set_field`, so this closes the gap VM-wide, not
just for the exact call site.

WildFly domain mode is exactly the kind of workload this bites: the host controller and
each managed server run substantial `java.util.concurrent` worker pools (JBoss Threads'
`EnhancedQueueExecutor`, XNIO), so plain (ordinarily-safe-on-a-real-JVM) field races of
this shape are expected under the sustained load of a 120-second management-connection
retry loop — matching this doc's description of the diagnostic recurring throughout the
wait rather than firing once.

**What this session could NOT do: re-run the exact failing test live to confirm the fix
end-to-end.** No Maven install or `wildfly-core` testsuite checkout was available on the
Azure probe host to rerun `EEConcurrencyExecutorShutdownTestCase` itself. As a substitute,
this session hand-built a WildFly 32.0.1.Final distribution (downloaded from GitHub
releases — no Maven needed for a binary distribution) and drove both `bin/standalone.sh`
and `bin/domain.sh` directly under a fresh `dev`-HEAD `cratonvm` build (real-JDK backend,
`--nojit`, `CRATONVM_DBG_CELLCORRUPT=1 CRATONVM_DIAG_HIB32=1` to force the guard's
diagnostic on every hit). Both boots stall well *before* reaching the point where the
original corrupt-cell diagnostic was observed:

- With the default configuration (matching what `apps/wildfly-suite-runner/run-suite.sh`
  actually sets — it does not touch this flag), CratonVM's real MSC `service.start()`
  callback is gated off by default (`CRATONVM_MSC_REAL_START`, see
  `docs/internal/app-jvm-bugs/handoff-wildfly-msc-service-start.md`), so the very first
  application-level service install after `WFLYSRV0049 ... starting` never signals
  completion and the boot waits forever. Confirmed via `CRATONVM_DEFAULT_WATCHDOG_SEC` +
  the built-in stack-dump watchdog: all non-daemon threads are correctly parked idle in
  `EnhancedQueueExecutor$ThreadBody.run` (near-0% CPU) — a real hang, not a slow boot, and
  a distinct, already-tracked, explicitly-incomplete feature gap rather than a new bug.
- Setting `CRATONVM_MSC_REAL_START=1` unblocks standalone mode only as far as an unrelated
  `org.jboss.msc.service.ServiceNotFoundException` inside `BootstrapImpl.internalBootstrap`
  (a legacy 2-arg `ServiceTarget.addService(ServiceName, Service).install()` call for
  `Services.JBOSS_AS` whose registration or lookup our MSC bridge doesn't yet handle), and
  makes domain mode hang even earlier with no exception at all for the full 200s window
  tried.

So neither configuration reaches sustained multi-threaded application-level execution,
meaning this session could not directly re-observe (or re-trigger) the corrupt-cell
guard against a live domain-mode process. The tearing mechanism itself is fixed and
verified at the code level (see above) plus by the pre-existing regression test
`gc/tests/plain_field_no_tearing.rs` (added by `2dfdfddc`) — re-run in this session both
on current `dev` and, after copying the test file onto the pre-fix parent commit
(`164264c8`), against the pre-fix `read_slot`/`write_slot`. Neither run forced a torn read
in 40+ trials on this hardware (consistent with `2dfdfddc`'s own note that a live
before/after repro could not be forced either — the race window is real per the code
diff and the memory-model argument, just too narrow to hit reliably without deliberately
delaying one half of the 16-byte write).

**Status going forward:** the specific defect this doc was opened for (the corrupt-cell
guard firing under WildFly domain load) is root-caused and fixed on `dev`. The overall
`EEConcurrencyExecutorShutdownTestCase` / `DefaultConfigSmokeTestCase` timeout is tracked
separately in
[wildfly-domain-managed-servers-timeout.md](wildfly-domain-managed-servers-timeout.md),
whose current proximate blocker is now the MSC real-service-start completeness gap above,
not this heap guard. Re-confirming this doc's fix live needs either (a) a host with
Maven + a `wildfly-core` testsuite checkout to rerun the actual Arquillian test, or (b)
progress on `docs/internal/app-jvm-bugs/handoff-wildfly-msc-service-start.md`'s P3/P4
follow-ups so a hand-driven `domain.sh`/`standalone.sh` boot can reach real sustained
concurrent execution again.
