# core/spring-boot-test — config-data loading gaps, duplicate classpath scan results, missing PropertySource (FIXED)

**Status: FIXED 2026-07-20** (branch `fix/sbtest-configdata-cluster-20260720`, merged to `dev`).

Original doc: `docs/known-issues/springboot/core-spring-boot-test-config-data-and-classpath-scan-cluster.md`
(found 2026-07-17). Re-investigated 2026-07-20 on the Azure Linux host against
current `dev` using an ad-hoc `SbRunner` single-class JUnit-Platform launcher
against `core/spring-boot-test`'s Gradle-generated classpath (a prior scratch
checkout, `springboot-jsonreader-deprecation-20260718`, already had the module
compiled with classes/resources on disk — reused read-only as a classpath/
fixture source, no edits made there beyond disposable debug tracing that was
reverted before this doc was written).

## Clusters A, B, D — already fixed by unrelated `dev` work

Re-ran all four classes covering clusters A, B, D against the current `dev`
tip baseline binary (no changes) — **all pass**:

- `ConfigDataApplicationContextInitializerTests` — 1/1 pass
- `ConfigDataApplicationContextInitializerWithLegacySwitchTests` — 1/1 pass
- `SpringBootTestCustomConfigNameTests` — 1/1 pass
- `SpringBootContextLoaderTests` (`propertySourceOrdering` + the other 25
  tests in the class) — 26/26 pass (1 skipped, expected)

No VM-source change was needed for these three clusters; between 2026-07-17
and 2026-07-20 `dev` picked up enough unrelated fixes (config-data loading,
`RandomValuePropertySource` composition, placeholder resolution) that these
symptoms no longer reproduce. Not root-caused to a specific commit — not
worth bisecting given they're independently verified passing now.

## Cluster E — re-verified passing, root cause was environmental, not a VM bug

`DuplicateJsonObjectContextCustomizerFactoryTests` (`@ClassPathOverrides`,
forks classpath via `ModifiedClassPathClassLoader`/Aether artifact
resolution) — ran **3 consecutive times** against the `dev`-tip baseline
binary: PASS every time, ~2.8s each, no hang.

- The `javax/net/ssl/SSLSocketFactory` `SyntheticStub` registry-drop-filter
  fix (`native-api/src/registry.rs:~3693`, referenced by the original doc)
  is still present and live.
- `org.json:json:20140107` (the Maven coordinate this test's
  `@ClassPathOverrides` resolves) is already cached in this host's local
  `~/.m2/repository`, so the Aether resolution step never needs live network
  I/O when re-run here.
- The original doc's "possible discrepancy" HANG on 2026-07-17 is most
  consistent with a **transient network-availability blip** during that
  specific rerun's first-time artifact download, not a persistent CratonVM
  correctness bug. No VM-level fix applied or needed; flagging this
  conclusion here so a future HANG on this exact class is not mistaken for a
  new regression without first checking Maven-artifact-cache/network state.

## Cluster C — original symptom FIXED; investigation found (and fixed) two
## deeper bugs in the same test; a third, harder residual remains OPEN

`SpringBootContextLoaderAotTests`. The **original** documented failure
(`IllegalStateException: Found multiple @SpringBootConfiguration annotated
classes` — a classpath-scan duplicate-resource-enumeration bug) **no longer
reproduces** on current `dev` — confirmed fixed independently, same as
clusters A/B/D.

However, the same test now fails with **three different, newly-uncovered**
signatures when run end-to-end (`loadContextForAotProcessingAndAotRuntime`,
which drives `TestContextAotGenerator` under `@CompileWithForkedClassLoader`
— a JUnit extension that reloads the whole test-class-and-framework graph
through a private forked `ClassLoader`, `org.springframework.core.test.
tools.CompileWithForkedClassLoaderClassLoader`, to isolate AOT-generated
code). Peeling back each layer revealed a *chain* of loader-identity bugs,
all specific to this classloader-forking scenario. Two are fixed; one
(harder, more architectural) is tracked as a new residual — see
[`docs/known-issues/springboot/spring-boot-test-forked-classloader-defineclass-namespace-collapse.md`](../../known-issues/springboot/spring-boot-test-forked-classloader-defineclass-namespace-collapse.md).

### Fix 1 — interface default-method dispatch ignored a concrete override under a loader-identity substitution (`vm/src/runtime/interpreter.rs`)

**Symptom (layer 1):** `UnsupportedOperationException: Invoke
loadContextForAotProcessing(MergedContextConfiguration, RuntimeHints)
instead` — thrown from `AotContextLoader`'s own 1-arg default method, called
from *its own* 2-arg default method, even though the receiver
(`SpringBootContextLoader`) declares a real, concrete override of the 2-arg
method. Reproduces under `--nojit` too (interpreter-level, not JIT).
HotSpot: passes cleanly.

**Root cause:** `AotContextLoader` declares two **overloaded** default
methods sharing a name but differing in arity —
`loadContextForAotProcessing(MergedContextConfiguration)` (default body:
throws, a guard against being called directly) and
`loadContextForAotProcessing(MergedContextConfiguration, RuntimeHints)`
(default body: calls the 1-arg overload — itself a template-method stub
meant to be shadowed by a real override). `SpringBootContextLoader`
overrides only the 2-arg overload.

`execute_invoke_kind`'s `loader_interface_override` logic (added for a
*different*, narrower purpose — giving a receiver-loader's exact copy of an
interface priority for identity-sensitive default methods like Spring's
`MergedAnnotation$Adapt`, under `@CompileWithForkedClassLoader`) substituted
`AotContextLoader`'s own (receiver-loader-exact) `class_id` as the dispatch
target **without first checking whether the receiver's concrete class chain
already provides a real override** for the exact method being called. That
substitution made `find_method_recursive` start its walk at the *interface
itself*, whose own default body (Phase 1, non-abstract, matches immediately)
wins over ever reaching `SpringBootContextLoader`'s override.

**Fix:** before applying the loader-interface-override substitution, check
`find_method_recursive(receiver_id, ...)` for a concrete (non-abstract,
non-interface-declaring) override; skip the substitution when one exists —
JLS/JVMS mandate the receiver's own override wins over any interface default
regardless of which loader's copy of the interface is "exact." Minimal
repro (`interface Iface { default go(s){throw} default go(s,n){go(s)} }
class Impl implements Iface { override go(s,n) {...} }`) passed on CratonVM
*before* this bug was fully understood — the bug only manifests with the
additional interface-hierarchy + `@CompileWithForkedClassLoader` receiver-
loader-identity substitution in play, which the minimal repro didn't
exercise (confirmed with a closer repro using an abstract-class + interface
hierarchy + reflective instantiation, which also didn't reproduce it —
final root-cause confirmation came from live debug tracing of the actual
failing dispatch inside the real Spring Boot Gradle checkout).

### Fix 2 — annotation enum-valued element materialization ignored the declaring class's loader (`native-builtins/src/lang_class.rs`)

**Symptom (layer 2, after fix 1):** `IllegalStateException: Main method not
found on 'SpringBootContextLoaderAotTests$ExampleConfig'` from
`SpringBootContextLoader.getMainMethod`, even though the test's
`@SpringBootTest` doesn't set `useMainMethod` (default `NEVER`, which should
make `getMainMethod` return `null` immediately via `if (useMainMethod ==
UseMainMethod.NEVER) return null;` before ever reaching the "main method not
found" assertion).

**Root cause (confirmed via live debug tracing — added temporary
`System.out.println` calls to a scratch copy of `SpringBootContextLoader.
getMainMethod`, recompiled just that file, reverted after):** the
`useMainMethod` value materialized from the merged `@SpringBootTest`
annotation (via `AnnotationElementValue::Enum` → `Enum.valueOf(Class,
String)`) was semantically `NEVER` but loaded by the **application**
classloader (`DeferredLogFactory.class loader=AppClassLoader`, an unrelated
enum with the same bug — the trace was on `UseMainMethod` specifically:
`useMainMethod` identityHash differed from `UseMainMethod.NEVER` as
referenced directly in `SpringBootContextLoader`'s own bytecode, which
resolved via the **forked** loader). `==` (the correct comparison for
enums) failed even though both printed `"NEVER"` and even `.equals()`
returned false (`Enum` doesn't override `equals`, so it's identity too) —
two distinct `Class` objects for the same enum, one per loader.

`annotation_element_to_java_typed`'s `AnnotationElementValue::Enum` arm
resolved the enum class purely by name (`ctx.class_id_by_name(class_name)`,
a loader-blind global lookup) — unlike the sibling `Class`-valued arm just
below it in the same function, which **already** had a `container_loader`
parameter threaded through specifically to resolve class-valued annotation
members through the declaring class's own loader
(`resolve_annotation_class_via_loader`, mirroring HotSpot's
`AnnotationParser.parseClassValue(sig, container)`).

**Fix:** thread the same `container_loader`-aware resolution into the
`Enum` arm — try `resolve_annotation_class_via_loader(ctx, loader,
class_name)` first (reusing the existing helper) and only fall back to the
global `class_id_by_name` path when no `container_loader` is available or
loader resolution fails.

### Residual (layer 3, OPEN) — classloader parent-chain fidelity / `defineClass` namespace collapse

After fixes 1 and 2, the test progresses much further (through
`SpringBootContextLoader.loadContext` into real `SpringApplication.run()`)
before failing with a **third** distinct signature:
`NullPointerException: Cannot invoke "DeferredLogFactory.getLog(Class)"
because "logFactory" is null` in `CloudFoundryVcapEnvironmentPostProcessor`'s
constructor, reached via `SpringFactoriesLoader`'s reflective
constructor-argument matching (`ArgumentResolver`/`FactoryInstantiator`).
Root-caused (not yet fixed) to a real, deeper classloader-fidelity gap in
CratonVM's `ClassLoader.loadClass` native delegation model. Along the way
this uncovered — and fixed as a real, low-risk, targeted improvement — a
related but not-quite-sufficient bug in `builtin_loader_reachable`
(`native-builtins/src/classloader.rs`): it treated *any* built-in loader
(including the **platform** loader, which can only see JDK modules) as
"safe to consult CratonVM's flat global class store," when only the
**application**-tier loader actually is. Fixed to exclude the platform
loader specifically (reusing the existing `is_platform_class_loader`
helper). This fix is real and independently justified by the function's own
documented JVMS-5.3 rationale, but is not sufficient alone to close this
test — see the residual doc for the fuller investigation and why it's
tracked separately rather than attempted as a quick fix in this session.

## Verification

All three fixes rebuilt+verified together (binary
`cratonvm-sbtest-cfgdata-clean-20260720`, `--release`, real-JDK mode,
`--java-home /home/victor/jdk25`) against `core/spring-boot-test`:

- All 6 originally-documented classes: **5/6 pass** (`SpringBootTestCustomConfigNameTests`,
  `ConfigDataApplicationContextInitializerTests`,
  `ConfigDataApplicationContextInitializerWithLegacySwitchTests`,
  `SpringBootContextLoaderTests` 26/26, `DuplicateJsonObjectContextCustomizerFactoryTests`
  all pass; `SpringBootContextLoaderAotTests` still fails on the layer-3
  residual above, tracked separately).
- A 25-class random regression sample spanning `context`, `json`, `system`,
  `web`, `mock`, `filter`, `assertj`, `bootstrap`, and `runner` subpackages
  of `core/spring-boot-test`: **25/25 pass**, 0 regressions from the 3
  changes.

## Affected classes (final status)

- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerTests` — PASS (unrelated fix)
- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerWithLegacySwitchTests` — PASS (unrelated fix)
- `core/spring-boot-test` | `SpringBootTestCustomConfigNameTests` — PASS (unrelated fix)
- `core/spring-boot-test` | `SpringBootContextLoaderAotTests` — original symptom FIXED; new residual OPEN (see linked doc)
- `core/spring-boot-test` | `SpringBootContextLoaderTests` — PASS (unrelated fix)
- `core/spring-boot-test` | `DuplicateJsonObjectContextCustomizerFactoryTests` — PASS (environmental, not a VM bug)
