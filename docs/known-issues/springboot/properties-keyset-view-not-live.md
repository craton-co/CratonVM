# FIXED — `Properties.keySet()`/`entrySet()`/`values()` returned disconnected snapshots, not live `Map` views

**Status: FIXED. `keySet()` fixed 2026-07-24 (second attempt; first attempt
reverted — see below). `values()` closed 2026-07-26 (see "Third fix: live
`values()` view" below) — `entrySet()` was already a live view by the time
`values()` was investigated, no separate fix needed for it.** General
`java.util.Properties` bridge gap (`native-builtins/src/
properties_sidetable.rs`), discovered via the Spring Boot core Cluster C
(logging bootstrap) batch. Was blocking `org.springframework.boot.logging.
log4j2.Log4j2LoggingSystemPropertiesTests
#appliesLog4j2RollingPolicyPropertiesWithDefaults` (now fully passing) and
contributing to several `LoggingApplicationListenerTests` failures (its
`@AfterEach` uses the exact idiom below; went from 22 to 17(->16, see below)
failures out of 41 once the `keySet()` fix landed — the rest are unrelated
to this specific gap, confirmed by direct inspection, see "Residual sweep"
below).

## Third fix (2026-07-26): live `values()` view

`entrySet()` (`native_properties_entry_set`) was already a genuine live view
by this point (a static entry-set view backed by the source `Properties`,
via `make_static_entry_set` — `entry.setValue(v)` / `iterator().remove()`
write through). `keySet()` was fixed above. But `values()`
(`native_properties_values`) still built a plain, disconnected `ArrayList`
snapshot (`build_value_list`) — the exact same defect class this doc
originally described for all three views, just not yet closed for this one.
Confirmed directly:
```java
Properties p = new Properties();
p.setProperty("x", "1"); p.setProperty("y", "2");
p.values().remove("2");
p.getProperty("y"); // expected null, got "2" before this fix
```

Fixed by tagging the `ArrayList` `values()` returns with the source
`Properties` object in its trailing (beyond-logical-size) capacity slot —
the exact same convention `native_map_values` (regular `HashMap`/
`Hashtable`) already uses, via a new shared helper,
`cratonvm_native_collections::make_live_values_list`. This reuses the
ALREADY-hardened generic `values_view_source` / `resync_values_view` /
`propagate_list_removal` machinery that `native_al_remove_obj`/
`native_al_clear`/the list iterator's `.remove()` already consult for
`java/util/ArrayList` — no new override on `ArrayList` itself (which would
carry the same "global override on a near-universal class" blast-radius risk
the `keySet()` retry above had to specifically avoid by scoping to
`LinkedHashSet`). `native_properties_values` could not reuse
`native_map_values` wholesale, because Properties data lives in a
process-wide Rust-side table (`properties_sidetable.rs`'s `table()`), not in
the real `Properties.map` CHM field `map_collect_values` reads for ordinary
maps — so it collects the same (side-table + CHM-exclusive) value set
`build_value_list`/`chm_extra_entries` already assembled, mirroring the
GC-pinning pattern already proven in `native_properties_entry_set`, then
hands that to `make_live_values_list` just for the live-view tagging.
`build_value_list` (now unused) was removed.

Verified (Azure Linux host, real JDK 25, release build):
`values().remove(v)`, `values().iterator().remove()`, and `values().clear()`
all write through to the source `Properties` now; a plain (non-Properties)
`ArrayList.remove()`/`.retainAll()` and a regular `HashMap.values().remove()`
are unaffected (the trailing-slot marker never matches an ordinary,
non-tagged list). `Log4j2LoggingSystemPropertiesTests` 3/3,
`JakartaApiValidationExceptionFailureAnalyzerTests` (the `keySet()` fix's
regression control) 2/2 — no regression.

**Known, pre-existing, OUT OF SCOPE limitation found while verifying this:**
`values().retainAll(...)` does NOT propagate to the source map for ANY
`Map.values()` view — not just `Properties`'. `native_al_retain_all` (the
shared `java/util/ArrayList.retainAll` native) never consults
`values_view_source` at all, unlike `native_al_remove_obj`/`native_al_clear`.
Confirmed with a plain `java.util.HashMap`: `new HashMap<>(Map.of("a","1",
"b","2")).values().retainAll(List.of("1"))` leaves `b` in the map. This is a
generic `ArrayList`/map-values-view gap, unrelated to the `Properties`
side-table bridge this doc tracks, so it is left unfixed here — flagged for
separate follow-up.

## Residual sweep (2026-07-26): confirmed the remaining `LoggingApplicationListenerTests` failures are unrelated

Re-ran the full Cluster C list on current `dev` + the `values()` fix above:
`LoggingApplicationListenerTests` 25/41 (16 failing, down from 17 — an
unrelated dev-drift improvement), `JavaLoggingSystemTests` 11/12,
`Log4J2LoggingSystemTests` 58/61, `SpringBootPropertySourceTests` 2/2,
`SpringProfileArbiterTests` 7/7, `DefaultLogbackConfigurationTests` 7/7,
`LogbackConfigurationAotContributionTests` 9/11. Inspected every failure's
assertion directly (not just the test name):

- All 16 `LoggingApplicationListenerTests` failures are `CapturedOutput`
  console-content leaking BETWEEN test methods (e.g. `parseLevelsNone`
  asserting output "not to contain" a previous test's `testaterror` log
  line, or `parseLevels` asserting output "to contain" `testatdebug` and
  getting `""`) — an `OutputCaptureExtension`/logging-appender state-reset
  gap, not a `System`-properties leak. Confirmed unrelated to this doc's
  bridge gap.
- `JavaLoggingSystemTests#withFile` — file-logging output assertion
  ("Expecting actual not to be empty"), unrelated.
- `Log4J2LoggingSystemTests`'s 3 failures — MDC correlation-ID padding
  format mismatch in the console/file pattern layout, unrelated.
- `LogbackConfigurationAotContributionTests`'s 2 failures — AOT reflection
  hint collection picking up extra `com.example.Alpha`/`com.example.Bravo`
  classes it shouldn't, unrelated.

None of these touch `Properties`/`System` properties view semantics — this
doc's own "the rest are unrelated to this specific gap" conclusion holds.

## Second attempt: scope the override to `LinkedHashSet`, not `HashSet`

The first attempt (see the original writeup below) registered natives on
`java/util/HashSet.retainAll`/`.remove` globally, gated on a
source-`Properties` side-table tag so ordinary `HashSet`s passed straight
through to real bytecode. That caused a real regression in an unrelated
test (`JakartaApiValidationExceptionFailureAnalyzerTests`, via a Spring
`DefaultSingletonBeanRegistry` interaction never fully root-caused) and
was reverted.

This attempt keeps the exact same tagging/propagation logic but changes
which class the `Properties.keySet()` snapshot is built as:
`java/util/LinkedHashSet` instead of `java/util/HashSet`
(`build_key_set`), with the native overrides registered on
`java/util/LinkedHashSet` specifically. `LinkedHashSet extends HashSet`,
so the `Set` contract and `instanceof HashSet` are unchanged for any
caller — but the override's blast radius shrinks from "every `HashSet` in
the entire process" to "objects this one function creates or the
override on `LinkedHashSet` retainAll/remove". Verified: full Cluster C
list + green controls, including
`JakartaApiValidationExceptionFailureAnalyzerTests` specifically (the
prior regression victim), all pass.

## Original writeup (root cause and first, reverted attempt)

General `java.util.Properties` bridge gap
(`native-builtins/src/properties_sidetable.rs`). The underlying gap is
general — any test relying on the common JUnit idiom
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

## First fix attempt — reverted (superseded by the `LinkedHashSet`-scoped retry above)

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
