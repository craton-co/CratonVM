# FacadeClassLoader.<init>: spurious Object.size() NoSuchMethodError + guarded Class-object OOB access

Status: fixed - archived

Date observed: 2026-07-04

Date fixed: 2026-07-05

## Resolution

Fixed in CratonVM by splitting `Unsafe.staticFieldOffset` from instance-field
synthetic offsets and by hardening the custom-handler `ClassLoader` resource
fallback so it only treats `ucp.path` as a URL list when `ucp` is a real
`URLClassPath` and `path` is a real `ArrayList`. The remote r7 probe binary
`cratonvm-keycloak-facade-oob-20260705-r7-ucpguard` removes both tracked
signatures in JIT-on and no-JIT runs:

- no `NSME_DBG` / `java/lang/Object.size()I`
- no `OOBFIELD_ASRTAG` / guarded `java/lang/Class` slot 24 access

The class still fails later with `org.hibernate.boot.MappingException: Could
not locate root element`; that is a separate residual Keycloak/Hibernate issue.

## Summary

During `io.quarkus.test.junit.classloading.FacadeClassLoader.<init>`
construction (Quarkus's JUnit test-discovery classloader facade), CratonVM
logs a `NoSuchMethodError` for `java.lang.Object.size()I`, immediately
followed by GC-guard warnings reporting an out-of-bounds field read *and*
write on what is genuinely a `java.lang.Class` object (`class_id=ClassId(12)`,
19 real fields, access at index 24):

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/Object.size()I"
  caller="io/quarkus/test/junit/classloading/FacadeClassLoader.<init>(...)V @pc=357"
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped
  (caller used slot index past receiver's layout — class layout is correct;
   the bug is in the caller's slot computation, typically a speculative
   collection-layout probe dispatched on a non-matching receiver type)
  obj=0x... index=24 num_slots=19 class_id=ClassId(12) class_name=java/lang/Class real_field_count=Some(19)
WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (...) value=Int(1)
```

**This looked, at first glance, like the cause of several test crashes** — it
does not run *only* on failing classes, but it is not the reason any of them
fail. Deep investigation (a targeted `CRATONVM_DBG_NSME` diagnostic added
temporarily, confirming the receiver of the failed `size()I` invoke is a
`java.net.URL` instance, plus a full Java-frame stack dump) traced the actual
call chain: `CustomLauncherInterceptor.initializeFacadeClassLoader()` →
`FacadeClassLoader(ClassLoader)` (1-arg ctor) → `FacadeClassLoader(6 args)`
(delegating ctor) — only 2 Java frames deep, no native frames in between, so
the `.size()` invoke happens directly inside the 6-arg constructor's own
bytecode. However, a full backtrace of the accompanying `Unsafe.getIntVolatile`/
`compareAndSwapInt` OOB-guard hits (via `CRATONVM_DBG_OOBFIELD=Class`) shows a
*different*, much deeper call chain: `native_unmod_for_each` →
`native_al_for_each` → `mh_dispatch` (a `MethodHandle`/lambda dispatch) →
interpreter → `Unsafe` CAS — generic JDK/`ConcurrentHashMap`-adjacent
machinery, not anything FacadeClassLoader-specific. The two symptoms likely
do not share one cause; both are logged near the same constructor because
Quarkus's test-discovery bootstrap exercises a lot of shared JDK internals in
a short span.

**Confirmed not the actual failure cause**: re-running the same test class
after fixing the two real blockers ahead of it in the boot sequence (see
`docs/internal/fixed-suite-bugs/smallrye-getconfigmapping-1arg-bare-interface-abstractmethoderror.md`
and the `junit:junit` classpath fix in `run-keycloak-suite.ps1`) shows this
exact WARN/guard sequence **still fires**, on every affected class, right
before the test progresses further and hits a completely different, later
failure (`SRCFG00013` Converter gap, see
`smallrye-config-missing-charset-memorysize-converters.md`). The guard drops
the bad access safely (no corruption, no crash) and execution continues
normally afterward.

## Scale

Directly observed on: `quarkus/deployment` (5 classes, all also hit the now-fixed
`TestConfig.classOrderer()` `AbstractMethodError` downstream) and
`tests/clustering` (`JdbcPingCustomSchemaTest`,
`compatibility.ClusteredOAuthClientTest`). Likely fires on every class that
goes through `FacadeClassLoader`/`CustomLauncherInterceptor` (i.e. most/all
Quarkus-based JUnit discovery in this suite) — the WARN is silent unless
`RUST_LOG`/tracing is at WARN level or above, so its true prevalence across
the full 1124-class run wasn't separately counted; it is not gated to only
failing classes.

## Why this is still worth tracking despite being non-fatal today

The GC guard is explicitly a *defensive* mechanism — its own comment says the
underlying bug is real ("the bug is in the caller's slot computation"),
just safely contained. A `java.lang.Class` object legitimately having an
`Unsafe.compareAndSwapInt` attempted against slot 24 (only 19 real fields)
means *some* code believes it's holding a different, larger object (a
collection/counter-bearing structure) at that point — an object-identity or
receiver-tracking bug somewhere in the `ConcurrentHashMap`/`ServiceLoader`/
lambda-dispatch chain reached via `Iterable.forEach`. Today it's caught and
dropped; a slightly different shape of the same bug (e.g. a write landing
in-bounds on a real field of the wrong live object, rather than out-of-bounds
on a dead slot) could silently corrupt real Java state without tripping any
guard.

## Next steps

1. Find what code performs `.forEach()`/`Iterable.forEach` on the object
   whose backing structure gets misidentified as a `java.lang.Class` — the
   backtrace (`native_unmod_for_each` → `native_al_for_each` →
   `mh_dispatch`) is the concrete lead; instrument
   `native-collections/src/lib.rs::native_al_for_each`/`native_unmod_for_each`
   (or the `Unsafe.compareAndSwapInt`/`getIntVolatile` natives in
   `native-builtins/src/lib.rs`) with an env-gated dump of the *Java* receiver
   type at the point of the CAS, not just the Rust call stack.
2. Separately confirm whether the `Object.size()I` NoSuchMethodError (receiver
   = `java.net.URL`) and the `Unsafe` CAS-on-Class OOB access are actually the
   same event or two independent things that happen to fire back-to-back in
   this constructor — the diagnostics added so far (see below) don't fully
   settle this.

## Diagnostics added while investigating (kept, env-gated, safe to leave)

`vm/src/vm/vm_exec.rs`, right after the existing `CRATONVM_DBG_NSME`
single-line diagnostic (~line 12374): dumps the full Java frame stack (up to
30 frames) at the point a `NoSuchMethodError` is about to be raised, mirroring
the pre-existing `CRATONVM_DBG_NPE_STACK` pattern for null-receiver invokes.
Zero behavioral change when the env var is unset.

## Repro

```
ssh victor@20.84.156.31
cd /data/wt-keycloak-full-20260704
CRATONVM_DBG_NSME=1 CRATONVM_DBG_OOBFIELD=Class \
  apps/keycloak-suite-runner/run-keycloak-suite.ps1 -Vm craton \
  -ClassList <(printf 'module\tclass\nquarkus/deployment\torg.keycloak.quarkus.deployment.PersistenceXmlDatasourcesTest\n') \
  -TimeoutSec 60 -RunName repro-facadecl-size \
  -Exe target/release/cratonvm-kcfull1124 -JdkHome /home/victor/jdk25
```

## Evidence

`/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/logs/quarkus_deployment.*.err.log`, plus ad-hoc debug runs at `.suite/results/dbg-nsme-stack-test/` and `.suite/results/dbg-oobfield-test/` on the same host/worktree.
