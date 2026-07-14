# WildFly standalone boot: stale ObjectRef residuals (FIXED)

Status: FIXED - 2026-07-13. This historical investigation is retained because it identified a broad moving-GC failure class and the final boot residuals. The complete remedy is in this change:

- invocation and native-call forwarding read barriers refresh every object argument after it leaves the operand stack and before dispatch/pinning;
- MSC 1.5 ServiceController.getService descriptors, DelegatingServiceController bridges, and the real ServiceController.Mode ordering/REMOVE transition are implemented;
- org/jboss/as/controller/ remains interpreted pending a focused invokespecial JIT backend correction, avoiding a proven uninitialized AbstractOperationContext.controllerOperations list during parallel boot.

Final Azure standalone validation used uniquely named binary cratonvm-wildfly-stale-objectref-final-20260712-0315-v12 and a unique Java shim. At 80 seconds it was alive with the management endpoint listening on 127.0.0.1:10813; HTTP returned 302, and the complete log contained none of [stale-objref], STW cross-thread, WFLYSRV0056, or controllerOperations. Focused descriptor and Mode tests pass. The full native-builtins suite in its intended legacy MSC scheduler compatibility configuration passed 2978 tests with only seven established unrelated failures.

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

## Follow-up session 3 (2026-07-11): systematic static-analysis sweep — ~37 more confirmed sites fixed across 6 files

Built the static-analysis sweep this doc's "Suggested next steps" (below) had been recommending since
session 2: a Python line-oriented scanner (not a real Rust parser — see its own docstring) over
`native-builtins/src`, `native-io/src`, `classloading/src` that:

1. Masks out comments/string literals (so braces/semicolons inside JVM descriptors or doc comments don't
   corrupt brace/statement matching).
2. Tracks `ObjectRef`-like local bindings: `let name = <stmt with Value::Object/ObjectRef, or a direct
   producing `ctx.*` call>`, plus `Value::Object(Some(name)) =>` match-arm / `if let` / `let-else`
   patterns.
3. **Two-pass wrapper-hazard/wrapper-producing discovery**: a first pass scans every `fn` in the three
   dirs for helper functions whose body itself calls a known GC-triggering `ctx.*` method (e.g.
   `native_module_get_class_loader`, `build_local_module_loader`, `alloc_concurrent_synthetic`) — these
   aren't literal `ctx.foo(...)` calls, so a naive scan misses them entirely; this pass promotes them to
   first-class hazards (and, when their return type is a bare `ObjectRef`, to producers too).
4. For each function, computes an interval-based "GC event between reset-point and use" check (a GC
   call's *closing paren line*, not its opening line, is what marks risk — an earlier, cruder version of
   this script that used the opening line produced a systematic false-positive class: a variable passed
   as one of a multi-line call's *own* arguments was wrongly flagged as "at risk from this very call").
5. Flags a bare use of a tracked, unpinned name if such a GC event falls strictly between it and its most
   recent (re)bind/pin-refresh, skipping `pin_native_root`/`read_native_pin` call arguments (the
   sanctioned reads) from the "use" scan.

First pass (direct `ctx.*` calls only) found 1968 candidates; adding the wrapper-hazard pass found 3247.
Per the scan's own design goal (**broad coverage over precision** — false positives are cheap to dismiss
on inspection, false negatives are the ones that cost a future session another live-repro hunt), this
count includes substantial noise (large registration/dispatch functions, `#[test]`-only code exercised
against the mock `NativeContext` where `pin_native_root` is a documented no-op, and control-flow-blind
false positives from branches that don't actually execute on the path to the flagged use — the scanner
has no CFG, so an early-return branch's own hazard call can spuriously "poison" a later, unrelated use).
Manually triaged the ~2200 candidates in files judged hot for WildFly boot / reflection / classloading
first, per the earlier suggestion; found and fixed **~37 confirmed real instances** (some consolidated —
see below) across:

- **`native-builtins/src/jboss_module_loader.rs`** (~15 sites) — the big one. Two directly on the
  hottest WildFly boot path: `build_local_module_loader` (the singleton boot `LocalModuleLoader` itself
  — held across its own `create_string` call before a `set_field`) and `native_loader_load_module`
  (`this` held across `extract_receiver_roots`'s internal `invoke_virtual`; `module` held across the
  RKC19/WF39 brute-force `ensure_class_initialized` pre-warm loop — gated on `is_brute_force_trigger`,
  i.e. exactly the `org.jboss.as.standalone` bootstrap-module path — before `register_var_handle_root`).
  Also: `build_resource_root_array`, `build_module_object` (`module` + the `loader` parameter, across
  4+ interleaved hazards), `native_module_get_class_loader` (its OWN receiver `this` — the exact function
  session 2 fixed *callers* of, which turned out to have the same bug on itself), `build_synthetic_url`
  (3 locals across 3 sequential `create_string` calls), `native_module_classloader_find_resources`,
  `build_empty_enumeration`, `native_module_classloader_get_resource_as_stream`,
  `native_module_get_module_loader`, `native_module_get_property_names` — plus 6 identical
  `alloc_concurrent_synthetic`+`create_string`+`set_field(exc,0,msg)` exception-construction sites
  (`throw_module_not_found` and 5 `ClassNotFoundException` throw sites), consolidated into one new
  shared `pub(crate) fn alloc_single_message_exception` helper (also reused from
  `classloader_real.rs`, 3 more sites) rather than repeating the pin dance inline 9 times.
- **`native-builtins/src/service_loader.rs`** (8 sites) — `native_sl_load_class` (the
  `ServiceLoader.load(Class)` static-factory entry point — `service`, the `Class` mirror, held across
  two `Thread.currentThread()`/`getContextClassLoader()` invokes), `impl_jars_load_class`,
  `native_stream_support_stream_from_spliterator` (3 locals — `spliterator`, `arr`, `snapshot` — across
  its own `new_array`/`ensure_class_initialized`/`alloc_object` calls), `alloc_synthetic_stream`,
  `native_stream_collector_accept` (a subtle one: `this`/`elem`/the OLD `storage` array all held across
  the array-growth `new_array` call inside the function's OWN growth branch), one more instance in
  `native_sl_stream`'s tail (missed by the otherwise very-carefully-already-pinned rest of that
  function — confirms a function "looking already hardened" can still have one remaining gap, per
  [[native-stale-local-family-and-persistent-singleton-roots]]), `drain_instances_to_stream` and
  `native_sl_find_first` (found by manual inspection while reading nearby code, not by the scanner
  itself — both hold an `Iterator` receiver across their own `hasNext`/`next` `invoke`/`invoke_virtual`
  calls).
- **`native-builtins/src/classloader.rs`** (6 sites) — `enumeration_from_url_strings` (same
  `build_synthetic_url`-in-a-loop shape as the jboss_module_loader.rs sibling), `ucl_add_url`'s array-growth
  branch, `ucl_new_instance`/`ucl_new_instance_parent` (the returned loader object, across `ucl_setup`),
  `ucl_setup` itself (`this` + the caller-supplied `url_arr`, across its own `new_array`), and
  `alloc_method_handle` (the shared `MethodHandle` builder used by every `Lookup.find*` native).
- **`native-builtins/src/classloader_real.rs`** (4 sites) — `init_classloader_common_fields` (`this`
  held across 7 sequential `alloc_concurrent_synthetic`/`new_object`/`invoke` calls populating
  `defaultDomain`/`classes`/`packages`/etc.), plus 3 `ClassNotFoundException` sites migrated to the new
  shared helper.
- **`native-builtins/src/lang_reflect.rs`** (2 sites) — `build_parameter_array` (the `Parameter[]`
  backing every `Executable.getParameters()` call — `arr`/`declaring_executable` across the per-parameter
  allocation loop) and `native_method_get_generic_exception_types`'s throws-array loop.
- **`native-builtins/src/lang_class.rs`** (2 sites, out of a much larger 116-candidate list only
  partially triaged — see "Not yet swept" below) — `native_constructor_new_instance`'s
  `ReflectionFactory.newConstructorForSerialization` branch (`obj`, the freshly-allocated deserialization
  target, held across the ancestor's real `invoke_special <init>` before being returned — this is the
  exact `"Constructor.newInstance: no declaring class"`-family failure mode this whole investigation is
  about, on a different, serialization-specific call path than the already-fixed
  `create_constructor_object`) and `build_serialized_lambda` (the lambda-`writeReplace()`
  `SerializedLambda` builder — same "many sequential `create_string` calls interleaved with reuse of the
  same freshly-allocated object" shape as the already-fixed `create_method_object`/
  `create_constructor_object`/`create_field_object`, just never given the same treatment).

All fixes use the established `pin_native_root`/`read_native_pin`/`unpin_native_roots` idiom, matching
the style of the already-fixed sites (pin every at-risk local immediately after it's produced/bound,
re-read the forwarded reference right before each subsequent use, unpin via the first pin's handle once
after the last use).

**Verification performed:** `cargo check -p cratonvm-native-builtins` clean; full
`cargo test -p cratonvm-native-builtins --lib` run locally (Windows worktree) — 2961/2965 relevant tests
pass; the 4 failures (`lang_class::tests::jspecify_type_use_*` ×3, one `nio_heap_byte_buffer_tests` test)
were confirmed via `git stash` to reproduce identically against the **unmodified** pre-fix code (the
JSpecify ones are flaky/order-dependent — 1-3 of the 3 fail depending on which subset runs — and the
ByteBuffer one fails deterministically either way), i.e. pre-existing, unrelated to this sweep. Rebuilt
release on the Azure host (`/data/data/wt-stale-objectref-sweep-20260711`, off `a87901e6` + these fixes,
~4 min build) and froze
`/data/data/frozen/frozen-cratonvm-stale-objectref-sweep-20260711-v1.bin`. Attempted the harness's own
10-class sample (`sample_results_v4.txt`'s exact class list) against it — every class now fails in
~10s with a bare, uncaught `org.jboss.modules.ModuleNotFoundException` before `Post-clinit fixup`
finishes, which looked like a severe regression until re-running the SAME repro
(`FlushOperationsTestCase`) against the untouched, previously-verified
`frozen-cratonvm-wf-surefire-boot-20260711-v5.bin` baseline reproduced the **identical** failure,
byte-for-byte. Root cause (confirmed, not this sweep's fault): the shared host's
`apps/wildfly/testsuite/integration/basic/target/wildfly/` distribution is missing its entire
`modules/` directory, and the Maven `build` module that would normally provision it
(`apps/wildfly/build/target/wildfly-32.0.1.Final`) doesn't exist on this checkout at all — i.e. the
provisioned server was never (re)built, or its target dir was swept, independent of any code in this
repo. Restoring just `modules/` from a generic `/data/data/wildfly-dist/wildfly-32.0.1.Final` extraction
found on the host did **not** fully resolve it (a real fix needs the actual `build` module's Galleon
provisioning re-run, a much larger and higher-risk undertaking on a busy shared host, out of scope for
this sweep) — left in place since it's strictly additive and can't make things worse for whoever
addresses the harness gap next. **This session's verification therefore rests on**: clean compilation,
the full unit-test suite, careful manual line-by-line review of each site against the documented
`pin_native_root` contract (mirroring the exact reasoning that validated all previously-fixed sites),
and byte-for-byte non-regression against the last known-good binary on the one live repro attempted
before the harness gap was found — not a live clean-boot demonstration. Whoever next has a working
harness on this host should re-run the `sample_results_v4.txt` 10-class sample (or the full suite)
against a rebuild of `dev` post-merge to get the real before/after numbers this sweep couldn't produce.

**Not yet swept**: this session prioritized the files most central to the WildFly boot-crash
investigation and stopped there given time budget — `lang_class.rs`'s other 114 candidates,
`lang_invoke.rs` (123), `servlet.rs` (49), `jboss_msc.rs`, `wildfly_core.rs`/`wildfly_naming.rs`/
`wildfly_security.rs`/`wildfly_undertow.rs`, `spring_startup_bootstrap.rs`, and the three giant
"Phase N native registrations" files (`lib.rs` 673 candidates, `phases_late.rs` 592, `phases_early.rs`
287 — a spot-check of `lib.rs` alone found another real, high-confidence bug,
`spring_xml_set_factory_bool`'s caller holding a `DocumentBuilderFactory` across its own repeated
`invoke_virtual` setter calls, not yet fixed) are still untriaged. The full JSON candidate list is not
preserved anywhere durable — a future sweep should re-run the scanner (design described above; not
committed to the repo, was a throwaway session script) rather than assume this list is exhaustive or
still accurate against a moved `dev` tip.

## Current state: this is a long-tail bug class, not a small fixed set of sites

Across the first two follow-up sessions, 6-7 sites of the identical defect were found and fixed
(`lang_class.rs` ×3 original, `stream_decoder.rs`, `stream_encoder.rs`, `jboss_module_loader.rs` ×2,
`service_loader.rs` ×1), on top of the ~169+5 sites a 2026-07-06 sweep already believed it had closed.
**Follow-up session 3** then built and ran the systematic static-analysis sweep this section used to
recommend (see above) and found **~37 more** confirmed sites across 6 files — a full order of magnitude
more than manual one-off chasing had found in the first two sessions combined, strongly confirming the
"long-tail bug class, not a small enumerable set" read below. Every fix has produced a real,
independently-reasoned improvement (matching the documented `pin_native_root` contract exactly), but the
sweep itself was only partially exhaustive (many candidate files, and the 3 giant "Phase N" registration
files, remain untriaged — see "Not yet swept" above) and could not be verified against a live WildFly
boot this session (unrelated harness environment gap, see above). This pattern (`ObjectRef` captured → N
allocating calls → used again, unpinned) is apparently common enough in this codebase's native
call-heavy code that manually auditing individual call sites one at a time, triggered by whichever
symptom happens to surface next, will likely never fully converge on its own — the sweep confirms a
recurring systematic pass (plus the debug-assertion hardening idea below) is the more productive
investment than more targeted symptom-chasing:

- ~~Write a simple static-analysis pass~~ — **DONE in follow-up session 3** (see above): found and fixed
  ~37 more sites this way. **Not fully exhaustive yet** — re-run over the untriaged files listed in
  "Not yet swept" above (the scanner script itself wasn't committed; re-author from this doc's
  description of its algorithm, or improve it further — e.g. it doesn't yet know about
  `ctx.initialize_class`/`ctx.new_object`/`ctx.define_class_full` as additional GC-triggering hazards,
  found by inspection but not added to the tool this session).
- ~~A debug-build assertion that validates every `ObjectRef` read...~~ — **IMPLEMENTED** later in
  follow-up session 3, for the `Generational` (default) backend: `CRATONVM_DBG_STALE_OBJREF=1` now turns
  a stale native-local read into a hard, deterministic panic instead of silent corruption, for one full
  GC cycle after the object is evacuated. See
  [[wildfly-stale-objectref-debug-assertion-scoping]] (`docs/internal/wildfly-stale-objectref-debug-assertion-scoping.md`)
  for the mechanism (turned out to reuse the GC's own existing forwarding-pointer header field rather than
  needing a new tombstone format — just a one-cycle quarantine delay on reclaiming evacuated memory) and
  its explicit scope boundaries (G1/ZGC not covered; a couple of narrower gaps around the JIT's
  guarded-inline fast path and `load_and_forward`'s own self-healing call sites). Verified via a new
  `gc/tests/stale_objref_debug_assertion.rs` integration test plus the full `cratonvm-gc` crate suite
  (823 tests) passing unchanged with the flag off.

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

1. ~~**Systematic**: write the static-analysis/grep sweep~~ — **done in follow-up session 3**; re-run it
   (or an improved version) over the still-untriaged files (`lang_invoke.rs`, `servlet.rs`, `jboss_msc.rs`,
   the `wildfly_*.rs` files, `spring_startup_bootstrap.rs`, and the three giant `lib.rs`/`phases_early.rs`/
   `phases_late.rs` registration files) rather than treating this sweep as exhaustive.
2. **First priority now**: get a WORKING WildFly harness on whichever host picks this up next — this
   session's own verification attempt was blocked by the shared Azure host's `apps/wildfly/build/target/`
   (the Galleon-provisioned server) simply not existing. Either re-run that Maven module's provisioning,
   or confirm a different host/checkout still has an intact one, before trusting any further sample runs.
3. Once a harness works: re-run `sample_results_v4.txt`'s exact 10-class sample against a fresh build of
   `dev` (which now includes this session's ~37 additional fixes) to get real before/after numbers this
   session couldn't produce, then re-run the earlier targeted-reproduction advice below if a residual
   remains: `CRATONVM_DIAG_SERVICELOADER=1` + `CRATONVM_DBG_STALE_RECV=1` together across enough retries
   (8+) of `FlushOperationsTestCase` or `SingletonReentrantTestCase`, copying `-output.txt` to a
   uniquely-named file immediately after each attempt (before the next attempt overwrites it) so a
   `WFLYCTL0153` hit's full diagnostic trail survives for analysis.
4. **STW stall** (lower priority, high risk): re-attempt live-gdb capture with poll-on-log-line timing
   (see above), per the already-thorough writeup in `wildfly-gc-barrier-boot-hang-and-harness-fixes.md`.
5. Once the residual is meaningfully smaller (or the systematic sweep is more exhaustive), re-run a
   full-suite round (not just a 10-class sample) for an accurate before/after count against the round-6
   baseline (583/605, 96%).

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

Follow-up session 3 (static-analysis sweep, 2026-07-11):
  Azure host 20.83.144.174, worktree /data/data/wt-stale-objectref-sweep-20260711 (branch
    fix/stale-objectref-sweep-20260711, off dev a87901e6)
  /data/data/frozen/frozen-cratonvm-stale-objectref-sweep-20260711-v1.bin — this session's frozen binary
  /data/data/wt-wf-surefire-boot-20260711/sample_results_stalefix_v1.txt,
    stalefix_run.log — the 10-class sample attempt that hit the harness environment gap (every class:
    ModuleNotFoundException in ~10s)
  Confirmed via a direct re-run of frozen-cratonvm-wf-surefire-boot-20260711-v5.bin (the last verified-good
    binary, predating this session) against the same FlushOperationsTestCase repro that the identical
    ModuleNotFoundException/timing occurs — i.e. the harness gap, not this session's code, causes it
  apps/wildfly/testsuite/integration/basic/target/wildfly/modules/ was entirely absent on this host;
    apps/wildfly/build/target/ (the Maven module that provisions it via Galleon) doesn't exist either.
    Partially (not fully) restored modules/ by copying from /data/data/wildfly-dist/wildfly-32.0.1.Final/
    (a plain distribution extraction found on the host) — did not resolve the ModuleNotFoundException;
    a real fix needs the actual build module's Galleon provisioning re-run, not attempted (out of scope,
    high risk on a busy shared host)
```


## Follow-up session 4 (2026-07-11): live harness restored; ServiceLoader + MSC callback stale-reference fixes

The Azure harness was made usable without changing tracked source: its missing
`apps/wildfly/build/target/wildfly-32.0.1.Final` provisioning output is an
ignored build artifact, so it now symlinks to the already-present
`testsuite/integration/basic/target/wildfly` distribution (including
`modules/`). This permitted real server-side retries again.

A crucial harness correction: setting `CRATONVM_BIN` changes the Surefire
**client** JVM only. Arquillian starts the WildFly server from
`-Dcontainer.java.home=<home>/bin/java`. Initial comparison attempts therefore
left the server on an older binary and are evidence only of the residual's
continued reproducibility. Subsequent runs used unique, SHA-256-verified
Java-home shims per frozen binary, so both the client and server executed the
intended build.

Two more high-confidence Family-1 sites were fixed:

- `native-builtins/src/service_loader.rs`: `discover_providers` now roots the
  `ServiceLoader`, service `Class`, and module/custom loader across Java
  dispatches; `load_provider_class` roots the loader across `create_string` +
  `findClass`/`loadClass`; `native_sl_iterator` roots the provider `Class`
  across its allocating empty-parameter-array creation before
  `getDeclaredConstructor`.
- `native-builtins/src/jboss_msc.rs`: the service object retained in the
  Rust-side `service_roots` map is refreshed after controller-mirror
  allocation, and `drive_starts` roots/re-reads both the service receiver and
  `StartContext` after dependency injection immediately before
  `Service.start`. This is directly on the `AbstractControllerService.start`
  path whose later boot step reports `this.controller is null`.

`cargo check -p cratonvm-native-builtins` passed after both changes, as did
release builds of the before/after and final probes. The corrected
server-side samples still reproduce the **independent** controller-null and
STW-takeover residuals (final MSC-root build: 3/4 controller-null, 1/4 STW),
so this issue remains open. The samples did not produce `WFLYCTL0153` after
these fixes, but the controlled current-dev sample also missed it in six
retries; that is encouraging but not enough to mark the extension residual
closed. The STW live-attach poll caught the warning but missed the process by
a narrow race again; no GC-barrier change was attempted.

## Follow-up session 5 (2026-07-12): real-MSC controller wiring and detector-guided boot hardening

This session used a fresh isolated source worktree and binary on the Azure
host:

```text
source: /data/data/wt-wildfly-stale-objectref-complete-20260711
binary: /data/data/target-wildfly-stale-objectref-complete-20260711/release/cratonvm
harness: /data/data/wt-wf-surefire-boot-20260711
```

The decisive controller-null finding was that the MSC shadow container had
been *fake-completing* services unless `CRATONVM_MSC_REAL_START` was set. In
that mode `AbstractControllerService.start(StartContext)` was never actually
called, so its `controller` field remained null and later
`ModelControllerImpl.getManagementModel()` deterministically raised the
documented NPE. The production behavior was changed so real MSC
`Service.start()` callbacks are enabled by default; setting
`CRATONVM_MSC_REAL_START=0` is retained only as a diagnostic escape hatch.

Enabling the real callback path exposed further genuine stale-reference sites,
all detected with `CRATONVM_DBG_STALE_OBJREF=1` and addressed using the normal
native-root / re-read discipline:

- `service_loader.rs`: provider class, constructor argument array, constructor,
  and provider instance lifetimes across reflective construction and
  `ArrayList.add`.
- `xnio_async.rs`: `OptionMap.Builder.set` object-value decoding before the
  allocation-capable builder/option lookup path.
- `native-collections`: HashMap put-chain, CHM get/segment-chain, CHM compute
  callbacks, and live HashSet view resynchronization paths.
- `jboss_msc.rs`: synthetic `ServiceController` operations needed by real MSC
  (`getService`, `getName`, `getServiceNames`) and the controller service
  object retained in the shadow container.

The real-MSC path also exposed an abstract-interface dispatch issue:
`ServiceController.getService()` initially continued to resolve to a code-less
method. Registering methods only under the interface/concrete class names was
not sufficient because native dispatch is class-keyed. The current source
therefore aliases `org/jboss/msc/service/ServiceController` onto
`ServiceControllerImpl` *after* all controller registrations are installed.

### Live verification performed

`cargo check -p cratonvm-native-builtins` passed repeatedly on Azure after the
changes, and each updated binary was built from the isolated target directory.
The server Java-home shim had the real-MSC opt-in removed, proving that the new
default, rather than a test-only environment variable, selected real starts.

The focused probe was:

```text
org.jboss.as.test.integration.jca.flushing.FlushOperationsTestCase
```

It progressed from immediate controller-null / interface / detector failures
to real starts past hundreds of MSC services (the trace reached service IDs
above 500, clustering services, JDBC registration, and transaction recovery).
The latest focused logs did not contain the earlier controller-null NPE,
`ServiceController.getService` `AbstractMethodError`, or a new stale-object
detector panic. They instead ended at Arquillian's managed-server startup
deadline:

```text
java.util.concurrent.TimeoutException:
  Managed server was not started within [60] s
```

One Azure-host reboot interrupted a detached long probe; the source worktree,
target binary, and harness artifacts survived. A later broad six-shard run was
started accidentally and immediately stopped; it is not evidence for this
issue. The intended six-shard regression run used the exact ten classes from
`sample_results_v4.txt`:

```text
BeanValidationTestCase
DefaultManagedThreadFactoryTestCase
DataSourceDefinitionTestCase
SharedBeanInEarsUnitTestCase
MetadataCompleteCustomDescriptorTestCase
OverriddenAppNameTestCase
SingletonReentrantTestCase
DisabledValidationTestCase
FlushOperationsTestCase
EjbRefLookupTestCase
```

All 10 currently fail by the same 60-second managed-server-start timeout
(rather than the original NPE / extension-race signatures). The focused result
directories are:

```text
/data/data/wt-wf-surefire-boot-20260711/out/
  wildfly-stale-regression-six-20260712-2038-s*-nojit-real-others-20260712-204321
```

### Current conclusion

The original controller-null mechanism has been identified and addressed: real
MSC starts must be the default. The stale-reference detector also no longer
reported a fresh failure in the latest focused controller-start traces.
However, a detector-clean *completed* WildFly server boot has not yet been
demonstrated: all ten targeted classes still hit the harness's 60-second
managed-server startup deadline. This issue must remain OPEN. The next session
should capture the live post-service-500 wait state (with GDB or the existing
STW census only after the timeout condition is observed) and distinguish a
remaining MSC dependency/liveness problem from a test-harness startup limit
before making further broad changes.

## Residual note 2026-07-13 (second session): one recurrence observed post-fix, not yet re-opened

A 15-attempt clean-host baseline sample (unrelated STW-hang investigation, same day) using a binary
built fresh from this fix's dev commit hit the WFLYCTL0153  symptom once (1/15) — the same signature 'Follow-up session 2'
above already characterized as a genuinely concurrent  race rather than a single
fixable unprotected-ObjectRef site. Consistent with a small residual rate, not a full regression of this
fix — noted here rather than reopening Status, but flagging for whoever next investigates WFLYCTL0153
recurrences.

## Follow-up session 4 (2026-07-14): scanner refined, 5 more WildFly-relevant files triaged

Continued the static-analysis sweep this doc's Follow-up session 3 started but left mostly untriaged.
Scanner refined this session to eliminate several false-positive classes found in the prior sweep's
output: match-arm mutual exclusivity, `Value::Object(None)` diverging-arm pollution, if-let/while-let
unrecognized binds, return-argument terminal hazards, and RHS-window truncation on heavily-commented
multi-line statements.

**Triaged and fixed this session** (496 insertions / 35 deletions, `dev` commit `e3d5fbb4`):
- `lang_class.rs`: `synthetic_class_mirror`, `illegal_arg_exc_null_to_primitive`,
  `wrap_as_invocation_target_exception`.
- `lang_invoke.rs` (the bulk of this session's fixes): `make_drop_arguments_adapter`,
  `alloc_method_handle`, `string_concat_render_value`, `alloc_string_concat_method_handle`,
  `mh_dispatch_filter`, `make_fold_adapter`, `mh_dispatch_fold`, `mh_dispatch_catch`, `mh_dispatch`
  (CONSTRUCTOR/GUARD/STRING_CONCAT/COLLECT/INVOKER arms), `lookup_find_special`, `record_deser_dispatch`,
  `build_method_type_from_descriptor`, `mhs_permute_arguments`, `mhs_guard_with_test`,
  `native_mhn_resolve`, `native_mhn_init`, `native_mhn_get_member_vm_info`.
- `jboss_msc.rs`, `wildfly_core.rs`, `wildfly_undertow.rs`: several more sites, same pattern.

**Verification**: `cargo check` clean; `cargo test -p cratonvm-native-builtins --lib`: 2983 passed / 7
failed, all 7 confirmed identical against the unmodified pre-fix baseline (`git stash`) — no regressions.
Merged to `dev` (`e3d5fbb4`), build-verified post-merge.

**Updated still-untriaged list** (subtract this session's coverage from Follow-up session 3's original
list):
- `lang_class.rs` — most of its ~116 candidates still unreviewed (only 5 total fixed across 2 sessions now)
- `lang_invoke.rs` — the ~16 functions above are fixed; the remainder of its 123 original candidates not
  yet individually re-confirmed against the refined scanner (worth a rerun — the earlier count used the
  cruder scanner and may over/undercount now)
- `servlet.rs` (49 candidates) — NOT started
- `spring_startup_bootstrap.rs` — NOT started
- `wildfly_naming.rs`, `wildfly_security.rs` — NOT started (only `wildfly_core.rs`/`wildfly_undertow.rs`
  got partial coverage this session)
- `lib.rs` (673 candidates), `phases_late.rs` (592), `phases_early.rs` (287) — still essentially
  unreviewed; these three giant files remain the largest unaddressed surface

**How to apply**: re-derive the scanner from this doc's methodology description (Follow-up sessions 3+4
combined) rather than starting over — the false-positive fixes from this session are worth preserving in
whatever script version comes next. Prioritize `servlet.rs` and `wildfly_naming.rs`/`wildfly_security.rs`
next (smaller, WildFly-boot-relevant, realistic to finish in one session) before attempting the three giant
Phase-N files.

**Addendum**: servlet.rs (14 functions, full pass) and spring_startup_bootstrap.rs (19 functions, full pass) were both fully triaged and fixed same-day by a sub-agent of this sweep session — commit 2d609591 (merged on top of e3d5fbb4/139e2644). Both files are now COMPLETE, not partial ("still-untriaged" list above should drop them). cargo test -p cratonvm-native-builtins --lib: 2983 passed / 7 failed, identical pre-existing baseline.

## WFLYCTL0153 CLOSED (2026-07-14) — root cause + fix

The `WFLYCTL0153: No META-INF/services/.../Extension found` recurrence flagged in the prior session's
addendum (and originally characterized across 3 earlier sessions as a genuinely concurrent
`DeferredExtensionContext` race) is now root-caused and fixed: `dev` commit `9b153844` (merging
`a0b0289a`, branch `fix/wflyctl0153-race-20260714`).

**Root cause**: several more Family-1 stale-ObjectRef-across-GC sites, root-caused via live
`CRATONVM_DBG_STALE_OBJREF=1 RUST_BACKTRACE=1` debugging against the isolated repro (poll-the-crash
technique, not static analysis this time):
- `native-collections/src/lib.rs`: `native_al_hash_code`, `native_map_put_evict_pinned`,
  `native_hashmap_get_exact`, `native_map_contains_key`, `comparator_compare` (the
  `ToIntFunction`/`ToLongFunction`/`ToDoubleFunction`/key-extractor-`Function` dispatch arms —
  functionally identical fix to one an independent concurrent session ALSO landed same-day as
  "Family-1 stale-ObjectRef fix (2026-07-13, follow-up)"; the merge conflict was comment-text-only, code
  was byte-identical), `lhm_init_with_cap` — HashMap/ArrayList/LinkedHashMap natives holding `this`/a
  search key/a bucket-chain `node` across a Java `hashCode()`/`equals()`/`compareTo()` dispatch (a
  moving-GC risk) without pinning.
- `vm/src/runtime/interpreter.rs`: `checkcast_lambda_instantiated_args` read a raw `args` slice element
  after a prior loop iteration's own GC-risking call, instead of reading back through the caller's
  already-established `native_pin_roots` handles.

**Verification**: iterative `CRATONVM_DBG_STALE_OBJREF` diagnostic loops (25 attempts each) went from
18/25 panics on an early candidate to 0/25 reproducing these specific call chains on the final candidate;
a follow-on 20-attempt PRODUCTION-mode (no debug flag) isolated repro of the original WFLYCTL0153 symptom:
**0/20 occurrences** (vs ~1/15 historical baseline — the exact symptom this doc has tracked across 3+
sessions). `cargo test -p cratonvm-native-builtins --lib` / `-p cratonvm-vm --lib` both clean vs baseline.

**Important caveat — the debug flag surfaced a MUCH larger remaining backlog, not fully mined**: even on
the final fixed binary, `CRATONVM_DBG_STALE_OBJREF` still panicked on a large fraction of repro attempts
(the fix above closes the sites that were reachable from THIS specific symptom's call chain, not the
whole boot path). Raw per-attempt logs with full backtraces from this session's iterative debugging are
preserved at `/data/data/wt-wflyctl0153-20260714-repro/out-fix2-diag/` and `out-fix3-diag/` (Azure host) —
each `HIT_staleobjref_N.log` has a full stack trace pinpointing an exact file:line. This is a rich,
live-confirmed data source for whoever continues the static-analysis sweep next: mining these logs for
distinct call sites (dedupe by the innermost non-generic frame, e.g. `grep -A20 'panicked at
gc/src/gen_heap.rs'`) will likely surface real sites faster than another blind static scan, though note
the logs span several iterations of an evolving fix candidate so not every panic in them is still live on
current `dev` — cross-check against the final commit's diff before assuming a given site is still open.
