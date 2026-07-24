# `Properties.keySet()`/`entrySet()`/`values()` return disconnected snapshots, not live `Map` views — `keySet().retainAll(...)`/`.remove(...)` silently no-op on the real properties

**Status: OPEN — found 2026-07-24.** General `java.util.Properties` bridge gap
(`native-builtins/src/properties_sidetable.rs`), discovered via the Spring
Boot core Cluster C (logging bootstrap) batch. Blocks
`org.springframework.boot.logging.log4j2.Log4j2LoggingSystemPropertiesTests
#appliesLog4j2RollingPolicyPropertiesWithDefaults` specifically, but the
underlying gap is general — any test relying on the common JUnit idiom
`Set<Object> baseline = new HashSet<>(props.keySet()); ...
props.keySet().retainAll(baseline);` to restore `System` properties between
test methods is affected if a prior method in the same process added keys.

## Symptom

`Log4j2LoggingSystemPropertiesTests` has the standard
`@BeforeEach`-snapshot / `@AfterEach`-`retainAll`-restore pattern.
`appliesLog4j2RollingPolicyProperties()` sets 7
`LOG4J2_ROLLINGPOLICY_*` system properties; its `@AfterEach` is supposed to
remove them via `System.getProperties().keySet().retainAll(baseline)`. The
next test method, `appliesLog4j2RollingPolicyPropertiesWithDefaults()`,
asserts none of those keys are present — and fails, because they leaked
through.

## Root cause (confirmed via minimal repro)

`native_properties_key_set` (`properties_sidetable.rs`) builds `Properties
.keySet()`'s return value as a **disconnected snapshot** — a fresh, real
`java.util.HashSet` populated by iterating the side-table
(`cratonvm_native_collections::make_hashset_with_elements` + manual
`.add()` calls) — not a live view backed by the same side-table the way
real JDK's `Hashtable.keySet()` is. The same applies to `entrySet()`/
`values()`.

Confirmed directly:
```java
Set<Object> baseline = new HashSet<>(System.getProperties().keySet());
System.setProperty("MY_KEY", "x");
System.getProperties().keySet().retainAll(baseline);   // no-op
System.getProperty("MY_KEY");                            // still "x"
System.getProperties().remove("MY_KEY");                 // works — direct Properties.remove is fine
```
`Properties.remove(Object)` (called directly on the `Properties` object,
not through a `keySet()` view) removes correctly — only the *view*-based
mutation path (`keySet().remove(...)`, `keySet().retainAll(...)`,
`keySet().iterator().remove()`) is disconnected from the source.

## First fix attempt reverted — do not retry without a much narrower scope

A native override on `java/util/HashSet.retainAll`/`.remove`, gated on a
side-table tag linking a specific snapshot `Set` back to its source
`Properties` (falling through to real bytecode via
`invoke_virtual_bytecode_only` for every untagged/ordinary `HashSet`), was
implemented and initially verified against the direct repro AND
`Log4j2LoggingSystemPropertiesTests`. However, it caused
`JakartaApiValidationExceptionFailureAnalyzerTests` (a previously-solid
green control, `@ClassPathExclusions`-driven, runs its test methods in an
isolated `ModifiedClassPathExtension` classloader) to fail two different
ways across two attempts:

1. First attempt: an `invoke_virtual_bytecode_only` argument-convention bug
   (passed the receiver as part of `args` when that method takes the
   receiver as a separate parameter and expects a params-only `args` slice)
   corrupted the real-bytecode fallback call for every *ordinary*
   (non-Properties-sourced) `HashSet.retainAll`/`.remove` call in the
   entire process — a change that touches this globally-used class is a
   large, easy-to-get-wrong blast radius.
2. After fixing the argument convention, a *different* failure appeared:
   `java.lang.IllegalStateException: Singleton
   'org...internalConfigurationAnnotationProcessor' isn't currently in
   creation` during Spring's own `AnnotationConfigApplicationContext`
   refresh — Spring's `DefaultSingletonBeanRegistry` tracks
   `singletonsCurrentlyInCreation` via a `Set`, and something about
   registering natives on `java/util/HashSet` globally (even correctly
   falling through to real bytecode for the untagged case) perturbed that
   unrelated code path. Not yet root-caused; could be identity-hash-on-
   every-HashSet-mutation overhead/side-effects, or a `Collections
   .newSetFromMap`-adjacent interaction.

Given a genuinely global class (`HashSet` is one of the most widely used
JDK collection types) regressed a previously-solid, unrelated green
control twice in a row, the fix was reverted in full
(`native-builtins/src/properties_sidetable.rs` restored to its pre-session
state) rather than risk an unverified regression shipping. **Any retry
must avoid a global `HashSet` override.** Better options to consider:

- Make `Properties.keySet()`/`entrySet()` return a genuinely live view
  (a purpose-built synthetic Set/Map-view class backed by the side-table
  directly, with its OWN class identity distinct from `java.util.HashSet`
  so no other `HashSet` usage in the process is affected).
- Or: natively override just `Properties.keySet()` to return something
  whose `retainAll`/`remove` are intercepted via a *different*,
  Properties-specific synthetic class name (not `java/util/HashSet`) so
  the override registration itself is inherently scoped and cannot leak
  into unrelated code paths.

## Repro

```java
import java.util.*;
public class SysPropRepro {
    public static void main(String[] args) {
        Set<Object> baseline = new HashSet<>(System.getProperties().keySet());
        System.setProperty("MY_TEST_LEAK_KEY", "hello");
        System.getProperties().keySet().retainAll(baseline);
        System.err.println(System.getProperty("MY_TEST_LEAK_KEY")); // expect null, get "hello"
    }
}
```
