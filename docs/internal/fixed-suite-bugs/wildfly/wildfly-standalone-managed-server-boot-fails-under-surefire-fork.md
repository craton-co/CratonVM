# WildFly standalone managed server fails to boot when launched from within a CratonVM Surefire fork — FIXED

Status: **FIXED** on dev (`../../../../native-builtins/src/lang_class.rs`, `create_constructor_object` /
`create_method_object` / `create_field_object`), 2026-07-11.
Severity: was **Critical** — the single dominant blocker for the WildFly suite under CratonVM
(583/605, 96% of `testsuite/integration/basic` failures in round-6).
Originally filed: 2026-07-11, Azure worktree `test/wildfly-full-suite-20260707`.
Fixed: 2026-07-11, worktree `fix/wildfly-surefire-boot-20260711`.

## Symptom (recap)

Almost every Arquillian `@RunWith(Arquillian.class)` test in `testsuite/integration/basic` failed
before any test method executed:

```text
org.jboss.arquillian.container.spi.client.container.LifecycleException: The java process starting the
managed server exited unexpectedly with code [1]
```

`target/wildfly/standalone/log/server.log` was never created — the managed server process died before
`jboss-modules` finished bootstrapping the logging subsystem. Elapsed time to failure was 5-25s.

The full investigation trail (ruled-out hypotheses: classpath-cap interaction, env-var inheritance,
stdout/stderr pipe deadlock) is preserved in git history at the pre-fix version of this doc
(`docs/known-issues/wildfly-standalone-managed-server-boot-fails-under-surefire-fork.md`,
commit `c7a9f514`).

## Root cause

`Constructor.newInstance()`/`Class.getDeclaredConstructor()` in real-JDK mode go through
`create_constructor_object()` (`../../../../native-builtins/src/lang_class.rs`), which:

1. `alloc_object()`s the synthetic `java.lang.reflect.Constructor` instance (`obj`).
2. Captures `class_mirror` via `get_class_mirror()` (can allocate — lazily creates the mirror on
   first use).
3. Builds the `parameterTypes`/`exceptionTypes` arrays via `build_mirror_array_comp()`, which resolves
   each parameter/exception type through `descriptor_to_class_mirror[_via_loader]()` — classloading,
   which can trigger a GC.
4. Only *after* all of that does it call `ctx.set_field_by_name(obj, "clazz", class_mirror)` etc.,
   using the **original, un-refreshed** `obj`/`class_mirror`/`param_arr`/`desc_str` locals captured
   before step 3.

`obj` (and the other locals) were held as raw `ObjectRef`s across every one of those GC-triggering
calls **without pinning**. Per the documented `pin_native_root`/`read_native_pin` contract
(`../../../../native-api/src/registry.rs`): "Native code that holds an `ObjectRef` across a re-entrant call ...
MUST pin it first: under the moving collector the object may relocate, leaving the raw `ObjectRef`
stale (it then resolves to a reused, usually `java.lang.Object`, slot)." `create_constructor_object`
(and the identically-structured `create_method_object` / `create_field_object`) violated this
contract — under light allocation pressure the collector rarely moved anything between allocation
and use, so the bug was latent and unnoticed.

WildFly's standalone-server bootstrap loads ~30-50 `Extension` SPI implementations via
`ServiceLoader.load(Extension.class)` — `native_sl_iterator()` (`service_loader.rs`) calls
`Class.getDeclaredConstructor()` + `Constructor.newInstance()` once per provider, immediately after a
`getResources(META-INF/MANIFEST.MF)` flat-classpath scan over WildFly's ~500-750 module jars
(see the WF32-fix cap in `classloader.rs`) and hundreds of intervening class loads. That volume of
classloading is exactly the GC pressure this bug needed: under the real Surefire-fork harness (a
full Arquillian/JUnit/WildFly-testsuite classpath already live in the parent, `jit-real` mode engaged),
a moving collection reliably landed *inside* `create_constructor_object` for several extensions —
observed via CratonVM's own diagnostic:

```text
[WARN] ServiceLoader: Constructor.newInstance failed with an internal VM error (not a
provider-specific reflective failure) -- skipping this provider ... error=Runtime(NullPointerException
{ message: Some("Constructor.newInstance: no declaring class") })
  provider=org.wildfly.extension.clustering.web.DistributableWebExtension
  provider=org.wildfly.extension.clustering.ejb.DistributableEjbExtension
  provider=org.wildfly.extension.io.IOExtension
  provider=org.wildfly.extension.elytron.ElytronExtension
  provider=org.wildfly.extension.security.manager.SecurityManagerExtension
```

("no declaring class" is exactly what `native_constructor_new_instance` throws when the `Constructor`
object's `clazz` field isn't a valid mirror — i.e. the stale-`ObjectRef` corruption described above.)

With Elytron (WildFly 32's default security subsystem), IO, and SecurityManager silently dropped from
the extension registry, `standalone.xml`'s `<subsystem>` elements for those extensions have no handler
to parse them. Boot proceeds far enough to finish `AbstractControllerService.registerModelController-
ServiceInitializationBootStep()` reaching `ServerService.boot()`, but the incomplete extension/model
setup this produces manifests, several frames later, as:

```text
ERROR [org.jboss.as.server] WFLYSRV0055: Caught exception during boot
    java.lang.NullPointerException: Cannot invoke "org.jboss.as.controller.ModelControllerImpl.getManagementModel()" because "this.controller" is null
        at org.jboss.as.controller.AbstractControllerService$1.run(AbstractControllerService.java:362)
        at org.jboss.as.server.ServerService.boot(ServerService.java:388)
        at org.jboss.as.controller.AbstractControllerService.registerModelControllerServiceInitializationBootStep(AbstractControllerService.java:640)
FATAL [org.jboss.as.server] WFLYSRV0056: Server boot has failed in an unrecoverable manner; exiting.
[cratonvm] System.exit(1) called
```

— exit code 1, before jboss-logmanager ever opens `server.log`, exactly matching the symptom. Isolated
shell-launched repros of the captured `java ... org.jboss.as.standalone` command never reproduced this
because a fresh, non-nested, non-Surefire-forked process has far less concurrent classloading pressure
on the parent side and simply never happened to land a GC in the danger window during the child's own
extension-loading loop.

## Fix

Pin every raw `ObjectRef` local (`obj`, `class_mirror`/`name_str`/`ret_mirror`/`type_mirror`,
`param_arr`, `exception_arr`, `desc_str`) immediately after it's produced, and re-read the forwarded
reference via `read_native_pin` right before each subsequent GC-risking call and right before the
final `set_field_by_name`/`set_field` block — mirroring the pattern `build_mirror_array_comp` already
uses internally. Applied to all three reflection-object constructors that shared the identical
unprotected pattern:

- `create_constructor_object` (directly implicated — used by `Class.getDeclaredConstructor()` /
  `getConstructors()`, and therefore by `ServiceLoader`'s provider instantiation).
- `create_method_object` (identical pattern; already had a separate, still-live
  `"Method.invoke: no declaring class"` error path for the same failure mode, just not yet observed
  at this frequency).
- `create_field_object` (identical pattern, smaller blast radius since `Field` reflection allocates
  less per-call, but the same hazard).

See the diff in `../../../../native-builtins/src/lang_class.rs` (`git log -S"no declaring class"` /
`git blame` on these three functions for the exact change).

## Verification

Built a fresh release binary from dev tip + fix (`fix/wildfly-surefire-boot-20260711`,
Azure host `/data/data/wt-wf-surefire-boot-src-20260711`) and re-ran the real Surefire/Arquillian
harness (not an isolated repro) against it:

- `org.jboss.as.test.integration.beanvalidation.BeanValidationTestCase`: previously failed in ~13s
  with the exact NPE above and no `server.log`. After the fix: **no more "internal VM error" ServiceLoader
  warnings anywhere in the run**, boots ~5x further (60s+, well past `WFLYSRV0049 starting`, subsystem
  parsing, Elytron/IO/connector subsystem `add` operations) before hitting a *different*, later-stage,
  unrelated failure (see Residual below).
- Broader sample of 10 previously-failing classes across `ee.*`/`ejb.*`/`jca.*` packages, same fixed
  binary: **6/10 now progress far past the original crash point** (60-70s wall time instead of
  8-13s, hitting the later-stage residual instead); 4/10 still hit the identical early
  `this.controller is null` crash. This is the expected partial-fix signature of a GC-timing-window
  bug — closing the highest-pressure trigger (ServiceLoader's extension-loading loop) reduces but does
  not eliminate the chance of landing in *some* unprotected-`ObjectRef` window during boot. See
  [[wildfly-parallel-boot-stale-objectref-residual]] (split off as a new known-issue) for the
  remaining cases.

Given the round-6 sample showed 583/605 failures with this exact signature, and 6/10 (60%) of a
representative re-sample now clear it, this fix is expected to recover the majority — but not all —
of that cluster. A full suite re-run is needed for an exact post-fix count; that re-run also needs to
separate out the pre-existing, now-more-visible bugs this fix "uncovers" further into boot (see
Residual).

## Residual (split off, not fixed here) — UPDATE 2026-07-11: 2 more sites found+fixed, 6/10 → 9/10

Fixing the ServiceLoader/reflection-construction GC-staleness bug did **not** fully close this
cluster on its own (6/10 in the sample below). A same-day follow-up session found the **identical**
unpinned-`ObjectRef`-across-`ensure_class_initialized`+`alloc_object` pattern in two more functions:
`native-io/src/stream_decoder.rs::alloc_stream_decoder` and
`native-io/src/stream_encoder.rs::alloc_stream_encoder` (both hold their `InputStream`/`OutputStream`
parameter across the same two GC-risking calls before storing it into the new
`StreamDecoder`/`StreamEncoder`'s field) — these back **every** `InputStreamReader`/
`OutputStreamWriter` construction VM-wide, so under WildFly's classloading-heavy boot they were at
least as impactful as the original three sites. Fixed the same way. Re-running the identical 10-class
sample against a binary with all three fixes raised the "clears the original crash" rate from **6/10 to
9/10** — see `docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` for the full
before/after breakdown.

The one remaining class in the sample does **not** deterministically hit the original crash — repeated
runs against the identical fixed binary produced 3 *different* outcomes (the original NPE, a
later-stage failure, and a *new* signature: `WFLYCTL0153: No META-INF/services/
org.jboss.as.controller.Extension found` for a *different* specific extension each time).

**UPDATE 2026-07-11 (second follow-up session): 2 MORE sites of the identical pattern found+fixed —
`native_module_load_service`/`native_module_load_service_from_caller_module_loader`
(`../../../../native-builtins/src/jboss_module_loader.rs`, held `service_type`/`service` across a lazy
`ModuleClassLoader` allocation) and `native_sl_iterator` (`../../../../native-builtins/src/service_loader.rs`, held
the reflective `Constructor` across `setAccessible` + an array allocation before its second use in
`Constructor.newInstance`).** The second one was caught directly in the act via a live
`CRATONVM_DIAG_SERVICELOADER=1` capture: the module-scoped provider lookup correctly found
`org.wildfly.extension.beanvalidation.BeanValidationExtension` (`providers=1`), but a *later* attempt to
instantiate that exact class failed with the same `"Constructor.newInstance: no declaring class"` this
whole investigation started with — proof that `WFLYCTL0153` was in some cases a downstream symptom of
the very same bug class, recurring at a different call site than the one originally fixed. Both new
fixes verified as real improvements (not regressions) but did **not** fully eliminate the residual (an
8-retry targeted sample after both fixes: 4/8 clear, 3/8 original NPE, 1/8 still `WFLYCTL0153`). See
`docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md`'s "long-tail bug class" section —
after finding 6-7 total sites of this exact pattern across 3 sessions with the residual still not fully
closed, that doc recommends a systematic static-analysis sweep over one-off manual chasing for whoever
continues this.

Separately confirmed still present (both follow-up sessions): the STW cross-thread JIT-takeover stall
(`wildfly-gc-barrier-boot-hang-and-harness-fixes.md`'s explicitly-flagged,
not-yet-fixed EnhancedQueueExecutor-parked-in-futex residual) — deep GC-barrier work already assessed as
high regression risk by a prior session; two live-gdb capture attempts across both follow-ups missed the
exact stall window (need to poll the log for the `rounds=64` warning before attaching, not a fixed sleep).

## Evidence

```text
Azure host 20.83.144.174, worktree /data/data/wt-wf-surefire-boot-src-20260711 (fix),
  /data/data/wt-wf-surefire-boot-20260711 (repro harness copy)
Frozen binaries: frozen-cratonvm-wf-surefire-boot-20260711-v1.bin (pre-fix, confirms repro),
  frozen-cratonvm-wf-surefire-boot-20260711-v2.bin (lang_class.rs fix only, 6/10 sample),
  frozen-cratonvm-wf-surefire-boot-20260711-v3.bin (+ stream_decoder.rs/stream_encoder.rs fixes, 9/10 sample),
  frozen-cratonvm-wf-surefire-boot-20260711-v4.bin (+ jboss_module_loader.rs service_type/service pin),
  frozen-cratonvm-wf-surefire-boot-20260711-v5.bin (+ service_loader.rs ctor pin, 8-retry targeted sample:
  4/8 clear, 3/8 original NPE, 1/8 still WFLYCTL0153 — real but incomplete improvement)
/data/data/cratonvm/apps/wildfly/testsuite/integration/basic/target/surefire-reports/
  org.jboss.as.test.integration.beanvalidation.BeanValidationTestCase-output.txt (before/after captured
  separately during the investigation)
/data/data/wt-wf-surefire-boot-20260711/sample_results_v3.txt (10-class before/after verdict log)
```
