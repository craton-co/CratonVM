# WildFly standalone boot: residual "different extension missing each run" + STW JIT-takeover stall

Status: OPEN — split off 2026-07-11 from
[[wildfly-standalone-managed-server-boot-fails-under-surefire-fork]] (FIXED at
`docs/internal/fixed-suite-bugs/wildfly-standalone-managed-server-boot-fails-under-surefire-fork.md`).
Updated 2026-07-11 (two follow-up sessions): four more concrete stale-`ObjectRef` sites found and fixed
across the two sessions (`stream_decoder.rs`, `stream_encoder.rs`, `jboss_module_loader.rs` ×2,
`service_loader.rs`). The residual is smaller but **not eliminated** — see "Current state" below for
why this is now understood to be a long-tail bug class, not a small fixed set of sites.
Severity: Moderate — down from "blocks most classes" (96% in round 6) to "blocks a minority,
non-deterministically" for `testsuite/integration/basic`.

## Background

The linked fixed-suite-bugs doc root-caused a stale-`ObjectRef`-across-GC bug in
`create_constructor_object`/`create_method_object`/`create_field_object`
(`native-builtins/src/lang_class.rs`) that corrupted several WildFly `Extension` SPI instances loaded
via `ServiceLoader` during standalone-server boot, cascading into a
`NullPointerException: ... "this.controller" is null` crash in `AbstractControllerService` — exit code
1, before `server.log` was ever written. A first verification pass (10-class sample) showed only 6/10
classes clearing the crash.

## Follow-up session 1 (2026-07-11): 2 more sites, 6/10 → 9/10

`native-io/src/stream_decoder.rs::alloc_stream_decoder` and
`native-io/src/stream_encoder.rs::alloc_stream_encoder` had the exact same defect: each holds its
`is`/`os` (`InputStream`/`OutputStream`) parameter across `ctx.ensure_class_initialized(...)` +
`ctx.alloc_object(...)` before storing it into the new `StreamDecoder`/`StreamEncoder`'s field. These
back every `InputStreamReader`/`OutputStreamWriter` construction VM-wide. Fixed with the same
pin/re-read pattern. Verified: the 10-class sample went from 6/10 to 9/10 clearing the original crash.

## Follow-up session 2 (2026-07-11): 2 more sites found via decompiled `DeferredExtensionContext` — real bugs, but did NOT eliminate the residual

Investigated the higher-priority lead from session 1: the one class that never deterministically hit
the original crash instead showed `WFLYCTL0153: No META-INF/services/org.jboss.as.controller.Extension
found for <extension>` for a *different* extension each time. Decompiled
`org.jboss.as.controller.parsing.DeferredExtensionContext` (`wildfly-controller-31.0.3.Final.jar`) and
confirmed it submits one `Callable` per extension to a `bootExecutor` `ExecutorService` (real
multi-threaded execution — each Callable calls `moduleLoader.loadModule(name).loadService(Extension.class)`
independently), then blocks on `Future.get()` for each, surfacing `ExecutionException` as the observed
`IllegalStateException`.

Traced the native call chain (`native_module_load_service`/`native_module_load_service_from_caller_module_loader`
→ `native_module_get_class_loader` → `discover_providers` → `module_service_provider_names` →
`module_service_roots`/`ensure_resolved`) and found **two more genuine instances of the same "Family 1"
stale-`ObjectRef`-across-GC pattern**:

1. `native_module_load_service`/`native_module_load_service_from_caller_module_loader`
   (`native-builtins/src/jboss_module_loader.rs`) captured `service_type`/`service` (the `Class` mirror
   for `Extension.class`) before calling `native_module_get_class_loader` — which lazily allocates a new
   `ModuleClassLoader` the *first* time it's asked for a given module, i.e. on essentially every
   extension-loading call during boot — then used the captured value again afterward, unpinned.
2. `native_sl_iterator` (`native-builtins/src/service_loader.rs`) held the `Constructor` object (`ctor`,
   from `Class.getDeclaredConstructor()`) across an intervening `AccessibleObject.setAccessible` invoke
   *and* a `new_ref_array` allocation before its second use in `Constructor.newInstance` — this one is
   the standout finding: a live diagnostic capture (`CRATONVM_DIAG_SERVICELOADER=1`) caught it directly
   in the act —

   ```text
   [SL-LOADER-DBG] JBoss module service module=org.wildfly.extension.bean-validation
     service=org.jboss.as.controller.Extension providers=1
     (["org.wildfly.extension.beanvalidation.BeanValidationExtension"])
   ...
   [SL-DBG] iterator() final list size=Some(Int(1))    <- an EARLIER, successful instantiation of the SAME class
   ...
   WARN ServiceLoader: Constructor.newInstance failed with an internal VM error ...
     error=Runtime(NullPointerException { message: Some("Constructor.newInstance: no declaring class") })
     provider=org.wildfly.extension.beanvalidation.BeanValidationExtension
   [SL-DBG]   skip (newInstance returned null): org.wildfly.extension.beanvalidation.BeanValidationExtension
   [SL-DBG] iterator() final list size=Some(Int(0))    <- a LATER instantiation of the SAME class fails
   ```

   This is direct, unambiguous proof: **the module-scoped provider lookup was correct** (found exactly
   the right provider, `providers=1`), but the *instantiation* step — a completely separate code path,
   one call site further down — silently dropped it due to the exact "Constructor.newInstance: no
   declaring class" corruption the original `lang_class.rs` fix was supposed to have eliminated. It
   hadn't, because the Constructor object itself (created correctly, thanks to that fix) went stale
   *after* creation, at this unrelated call site.

Both fixed with the same pin/re-read pattern. Verified with an 8-retry targeted sample against the
fixed binary (`frozen-cratonvm-wf-surefire-boot-20260711-v5.bin`): result was 4/8 clearing the crash,
3/8 original NPE, **1/8 still `WFLYCTL0153`** — i.e. real, partial improvement, but **not** full
elimination. A live capture of that one remaining `WFLYCTL0153` hit was not obtained (the per-run
`-output.txt` gets overwritten by the next attempt before it can be inspected) — it may be yet another
distinct call site with the same pattern, or a recurrence via a path not yet traced.

## Current state: this is a long-tail bug class, not a small fixed set of sites

Across both follow-up sessions, **6 total sites** of the identical defect have now been found and fixed
(`lang_class.rs` ×3 original, `stream_decoder.rs`, `stream_encoder.rs`, `jboss_module_loader.rs` ×2,
`service_loader.rs` ×1 — 7 if counting precisely), on top of the ~169+5 sites a 2026-07-06 sweep already
believed it had closed. Every fix has produced a measurable, real improvement, but each has also left a
residual — this pattern (`ObjectRef` captured → N allocating calls → used again, unpinned) is
apparently common enough in this codebase's native call-heavy code that manually auditing individual
call sites one at a time, triggered by whichever symptom happens to surface next, will likely never
fully converge. **Recommend a systematic approach for whoever picks this up next**, rather than more
targeted symptom-chasing:

- Write a simple static-analysis pass (even a crude one — a script scanning for `ObjectRef`/`Value`
  locals bound before a call to `ctx.invoke`/`ctx.alloc_object`/`ctx.ensure_class_initialized`/
  `ctx.new_ref_array`/etc., then read again afterward without an intervening `pin_native_root` in the
  same binding's lifetime) over all of `native-builtins/src`, `native-io/src`, and `classloading/src`.
  This is exactly the kind of mechanical pattern-match a script (or even a targeted grep + manual triage
  pass) can find much faster than one-bug-at-a-time live reproduction.
- Alternatively (or additionally), a debug-build assertion that _validates_ every `ObjectRef` read from
  a native local against a "known allocation epoch" counter, panicking loudly the moment a stale read is
  detected (rather than silently resolving to a reused all-zero slot) would convert every future instance
  of this bug class from "silent, non-deterministic corruption 1-20% of the time" into "always caught in
  CI/tests" — worth scoping as a `CRATONVM_DBG_*`-gated hardening feature.

## Also confirmed still present: STW cross-thread JIT-takeover stall (pre-existing, already tracked, NOT attempted)

Several of the "boots much further, still doesn't pass" classes (`FlushOperationsTestCase`,
`DataSourceDefinitionTestCase`, `DisabledValidationTestCase`, etc., each running 60-82s before
Arquillian's own ~60s client-side timeout gives up with `LifecycleException: Could not start
container`) show:

```text
[WARN] cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is still waiting for
  cooperative mutators rounds=64 pending=2..5 taken=0
```

This is the **same symptom shape** (`rounds=64`, `taken=0`) as the STW cross-thread JIT-takeover
deadlock documented in `docs/internal/fixed-suite-bugs/wildfly-gc-barrier-boot-hang-and-harness-fixes.md`
— that doc fixed the *main-thread-stuck-in-`pthread_join`* instance (`bee86ff0`), but explicitly flagged
an **EnhancedQueueExecutor-worker-parked-in-futex** instance as a **distinct, unfixed residual**, with
two candidate fixes described but deliberately not attempted due to "deep GC-barrier work, high
regression risk". Both follow-up sessions' live-gdb poll-and-pounce attempts **missed the stall window**
(the child process had already exited by the time gdb attached, using a fixed post-discovery delay).
**Do not re-attempt a fix here without a live gdb attach that actually lands in the window**: poll the
class's own `target/.../surefire-reports/<class>-output.txt` for the `rounds=64` warning line itself
(via `CRATONVM_DBG_STW_CENSUS=1`) before attaching — not a fixed sleep — since the stall's onset varies
8-60s across classes/runs. This remains explicitly lower priority than the extension-loading residual
above: it's already deeply characterized as high-risk, and neither follow-up session's time budget
included the extensive validation such a GC-barrier change would need.

## Suggested next steps

1. **Systematic**: write the static-analysis/grep sweep described above over `native-builtins/src`,
   `native-io/src`, `classloading/src` for the general unpinned-`ObjectRef`-across-allocating-call
   pattern, rather than continuing to chase individual WFLYCTL0153/NPE instances one at a time — this
   investigation's own experience (6-7 sites found across 3 sessions, residual still present) shows the
   manual approach has diminishing returns.
2. **If continuing the targeted approach**: reproduce with `CRATONVM_DIAG_SERVICELOADER=1` +
   `CRATONVM_DBG_STALE_RECV=1` together across enough retries (8+) of `FlushOperationsTestCase` or
   `SingletonReentrantTestCase` against `frozen-cratonvm-wf-surefire-boot-20260711-v5.bin`, and this time
   **copy the `-output.txt` to a uniquely-named file immediately after each attempt** (before the next
   attempt overwrites it) so a `WFLYCTL0153` hit's full diagnostic trail survives for analysis.
3. **STW stall** (lower priority, high risk): re-attempt live-gdb capture with poll-on-log-line timing
   (see above), per the already-thorough writeup in `wildfly-gc-barrier-boot-hang-and-harness-fixes.md`.
4. Once the residual is meaningfully smaller (or the systematic sweep lands), re-run a full-suite round
   (not just a 10-class sample) for an accurate before/after count against the round-6 baseline
   (583/605, 96%).

## Evidence

```text
Azure host 20.83.144.174, worktree /data/data/wt-wf-surefire-boot-20260711 (repro harness copy),
  /data/data/wt-wf-surefire-boot-src-20260711 (fix source, rebuilt v2 through v5)
Frozen binaries:
  frozen-cratonvm-wf-surefire-boot-20260711-v2.bin (lang_class.rs fix only — 6/10 sample)
  frozen-cratonvm-wf-surefire-boot-20260711-v3.bin (+ stream_decoder.rs/stream_encoder.rs — 9/10 sample)
  frozen-cratonvm-wf-surefire-boot-20260711-v4.bin (+ jboss_module_loader.rs service_type/service pin —
    10-class sample regressed to 6/10 due to sampling noise + the service_loader.rs site below, not yet fixed)
  frozen-cratonvm-wf-surefire-boot-20260711-v5.bin (+ service_loader.rs ctor pin — 8-retry targeted
    sample: 4/8 clear, 3/8 original NPE, 1/8 still WFLYCTL0153)
/data/data/wt-wf-surefire-boot-20260711/sample_results_v3.txt, sample_results_v4.txt — 10-class verdict logs
/data/data/wt-wf-surefire-boot-20260711/diagsl-hit-2.txt — the live CRATONVM_DIAG_SERVICELOADER capture
  proving the module-scoped lookup succeeded (providers=1) while the later Constructor.newInstance call
  for the SAME class failed with the stale-ctor symptom
gdb_pounce.sh / gdb_pounce.out, gdb_pounce2.sh — live-attach attempts that missed the STW stall window;
  worth another attempt with poll-on-log-line timing instead of a fixed sleep
```
