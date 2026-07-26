# Keycloak SmallRye Assert.assertNotNull linkage crashes

Status: fixed (classpath gap closed; a separate, pre-existing, already-tracked
quarkus-core gap is what remains — see Follow-on issue).

Date observed: 2026-07-02
Date fixed: 2026-07-02

## Summary

The expanded Keycloak `tests` and `testsuite` CratonVM run recorded 37
`CRASH` rows in `testsuite/model` with this linkage warning:

```text
NoSuchMethodError
method="io/smallrye/common/constraint/Assert.assertNotNull(Ljava/lang/Object;)Ljava/lang/Object;"
caller="org/keycloak/config/OptionBuilder.expectedValues(Ljava/util/List;)Lorg/keycloak/config/OptionBuilder; @pc=4"
```

The final process error then reports:

```text
[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error:
no class def found: org/keycloak/testsuite/model/KeycloakModelTest
```

## Root cause

`io/smallrye/common/constraint/Assert` was entirely absent from
`apps\keycloak\kc-universal-cp.txt` — not merely a version mismatch (`javap`
confirmed the method exists in every available
`smallrye-common-constraint-*.jar`; the jar itself just wasn't on the
classpath). That absence is why the reported error is a confusing
`NoSuchMethodError` rather than a plain "class not found":

CratonVM's classloader (`../../../../classloading/src/class_manager.rs`,
`is_enterprise_stub_prefix` / `is_jdk_class` / `create_synthetic_stub`)
silently fabricates an empty synthetic stub class for any unresolvable class
under a curated "enterprise" package-prefix allowlist (`org/jboss/`,
`org/wildfly/`, `org/xnio/`, `org/infinispan/`, `io/quarkus/`, `io/agroal/`,
`io/undertow/`, `io/smallrye/`) instead of raising
`ClassNotFoundException`/`NoClassDefFoundError` — this is deliberate,
load-bearing behavior for the WildFly/Keycloak/JBoss-Modules app gauntlet
(see `docs/internal/CRATONVM_BUGS/BUG-G-classforname-never-throws-cnfe.md`),
not a bug in itself. The fabricated stub has no real methods (only a few
special-cased constructors for `Throwable`-like/`Proxy$Instance` names), so
`Assert.assertNotNull` resolved against the stub always fails as
`NoSuchMethodError`, which then cascaded into `OptionBuilder`'s static
initialization failing, and ultimately into a hard top-level "linkage error"
process abort before JUnit could run or report anything.

## Fix

`apps\keycloak\kc-universal-cp.txt` is a local, gitignored, machine-generated
file (not checked in, no in-repo generator script), so the fix was applied
directly to that file on this machine: appended
`smallrye-common-constraint-2.16.0.jar` (matching the version already used by
`smallrye-common-annotation-2.16.0.jar` on the same classpath), from the local
Maven repo (`~/.m2/repository/io/smallrye/common/smallrye-common-constraint/2.16.0/`).

Verified with a direct repro (`KcRunner
org.keycloak.testsuite.model.client.ClientModelTest` and
`org.keycloak.testsuite.model.authz.ConcurrentAuthzTest` on a freshly built
`cratonvm.exe`): the `Assert.assertNotNull` `NoSuchMethodError` no longer
occurs, confirmed independently by an adversarial re-verification pass that
reran the exact repro from scratch.

### Diagnostics added (so this class of bug self-diagnoses next time)

Two small, log-only, opt-in additions (reviewed and confirmed lock-safe /
panic-safe / behavior-preserving by an independent adversarial pass — no
control flow, return value, or thrown-error type changed):

- `../../../../classloading/src/class_manager.rs`: a new `CRATONVM_TRACE_UNIMPLEMENTED`-gated
  `eprintln!` right at the synthetic-stub fallback site, printing exactly
  which class was not found on any classpath entry (this was previously only
  a `debug!` call, and the `tracing` crate here is built with
  `max_level_info`, so it never actually printed in a release build even with
  `RUST_LOG=debug`).
- `../../../../vm/src/vm/vm_exec.rs`: the existing terminal `NoSuchMethodError` warning
  now appends a `" [class not found on any classpath entry — synthetic
  stub, add the missing jar]"` hint to the `method` field when the target
  class is a synthetic stub, so a masked classpath gap is distinguishable
  from a genuine method-resolution bug without a debug rebuild.

## Follow-on issue (separate bug, uncovered by this fix)

With the `assertNotNull` crash resolved, `testsuite/model` classes
(`ClientModelTest`, `ConcurrentAuthzTest`, and every other class that extends
`KeycloakModelTest`) still fail — but now as a *different*, pre-existing,
already-tracked gap:

```text
[cratonvm] stub fallback: io/quarkus/opentelemetry/runtime/config/build/SamplerType — not found on any classpath entry
[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error: no class def found: org/keycloak/testsuite/model/KeycloakModelTest
```

This is the same `quarkus-core` classpath gap already tracked in
[`keycloak-testframework-quarkus-config-classpath-gap.md`](keycloak-testframework-quarkus-config-classpath-gap.md)
(that doc names `CharsetConverter`/`MemorySizeConverter`/
`InetSocketAddressConverter`; `SamplerType` is another `quarkus-core`
class in the same missing dependency, reached via a different static-init
path). Not a new bug — no new doc needed; the quarkus-core doc's own next
steps (identify and add the full `quarkus-core` transitive jar set) cover
this too.
