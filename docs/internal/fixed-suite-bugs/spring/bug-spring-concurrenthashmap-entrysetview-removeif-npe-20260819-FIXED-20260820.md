# FIXED — `ConcurrentHashMap.entrySet().removeIf(...)` threw NPE, and it cost 67 Spring Framework classes

## Status
**FIXED 2026-08-20** on `fix/spring-chm-clusters-20260820`. Filed 2026-08-19
against the three-GC-variant Spring Framework sweep
(`/data/sb-fw-3gc-out/fw3gc-20260819-193219`, 2,848 classes, generational/G1/ZGC,
6-way sharded each), where it was the largest single cluster among the 90
classes that failed under all three collectors.

Measured A/B on the Azure host, same 90 classes, same driver, same box, with
only the binary changed:

```text
                      classes OK / 90
  dev tip 26e4b5db4        15
  + this fix               83
```

Zero regressions: every class OK on the base binary was still OK on the fixed
one. The remaining seven at that point are the subject of the sibling write-up
(the retired `bug-spring-remaining-fail-clusters-20260819` doc); after the fixes
described there the same 90 stand at **88 OK**.

Behavioural oracle: `probes/ViewRemoveIfProbe.java` +
`probes/ViewRemoveIfProbe.expected.txt` — 40 rows over six map families and four
set families, 3 failing before, 0 after. Registration guard:
`set_view_carrier_remove_if_registered` and
`set_view_carrier_mutators_registered` in native-collections.

## Landed concurrently, twice

`dev` fixed this independently while this branch was measuring it: the
`native_hs_remove_if` registration arrived through the
`fix/jetty-jsp-and-groovy-mh-20260819` merge (`82a95f48a`, "…and two more
gate-without-registration defects"), whose own body is the same shape as this
branch's — same ordered snapshot, same `native_hs_remove` write-through, a
closure where this one used a helper function. The merge keeps THAT
implementation; this branch contributes the two registrar tests
(`set_view_carrier_remove_if_registered`,
`set_view_carrier_mutators_registered`), the behavioural oracle
(`probes/ViewRemoveIfProbe.java`) and the 90-class measurement below.

Worth recording rather than tidying away: two sessions reached the same
one-line diagnosis from opposite ends — one from a Spring Boot
`containersFailed=1` with every test passing, one from 67 Spring Framework
classes — which is what a defect class this quiet looks like from the outside.

## The bug

```java
ConcurrentHashMap<String, String> map = new ConcurrentHashMap<>();
map.put("a", "1"); map.put("b", "2"); map.put("c", "3");
map.entrySet().removeIf(e -> e.getKey().equals("b"));
```

```text
HotSpot   → true, size 2
CratonVM  → java.lang.NullPointerException: Cannot invoke
            "java.util.concurrent.ConcurrentHashMap.removeEntryIf(
            java.util.function.Predicate)" because "this.map" is null
              at java.util.concurrent.ConcurrentHashMap$EntrySetView
                 .removeIf(ConcurrentHashMap.java:4856)
```

on the very first call, every time.

## Root cause: a force-native gate entry with nothing registered behind it

CratonVM does not store `ConcurrentHashMap`'s entries in the real JDK's `table`
array; a view over one is minted under a carrier class
(native-collections' `SET_VIEW_CARRIERS`) whose state lives in this VM's own
slots. Both force-native gates —
`force_native_over_real_jdk_bytecode` in
`vm/src/runtime/interpreter/native_override.rs` and its companion inside
`vm/src/vm/vm_exec.rs` — have listed `removeIf` for those carriers since the
carriers were introduced, so the intent was never in doubt.

What was missing is the other half. `register_set_view_carrier_natives`
registered `size`, `contains`, `remove`, `iterator`, `forEach`, `stream`,
`spliterator`, `addAll`, `removeAll`, `retainAll`, `containsAll`, `equals`,
`hashCode` — **and not `removeIf`**. And a force-native gate with nothing
registered behind it does not throw: `admit_forced_native`'s `Ok(None)` arm
declines the interception and the call proceeds to the real JDK bytecode. The
gate said "this must be native", the registry had no native, and the JDK body
ran anyway.

That only bites on one of the seven carriers. `ConcurrentHashMap$EntrySetView`
is the only one whose own JDK class DECLARES `removeIf` (checked with `javap`
across all seven); the other six inherit `Collection`'s default, which walks the
native `iterator()` and already worked. `EntrySetView`'s declared body is
`return map.removeEntryIf(f)` over a `final` field a CratonVM-minted view leaves
null — hence the NPE, and hence a bug that looks GC-independent and
dispatch-shaped because it is neither: it is one missing line in a registrar.

The 2026-08-19 triage guessed an `invokeinterface`-vs-`invokevirtual` dispatch
gap between the two copies of the class/method list. That guess was wrong in an
instructive way: the two lists AGREE on `removeIf`, and comparing them against
each other could never have found this. The list they both had to be compared
against was the registrar's.

## The fix

`native-collections/src/lib.rs`:

* `native_hs_remove_if` — the JDK's semantics over the view's ORDERED snapshot
  (`collect_view_snapshot_ordered`, the same one `iterator()`/`forEach()` hand
  out, so the predicate sees entry objects for an entrySet view and keys for a
  keySet one), deleting matches through `native_hs_remove`, which already
  carries the per-family write-through: `map.remove(k)` for a keySet view and
  the `map.remove(e.getKey(), e.getValue())` contract for an entrySet one.
* Registered on **every** `SET_VIEW_CARRIERS` entry, not just the one that was
  broken, so all seven take one path and the registrar matches the gate.

The body pins `this` and the predicate before the first GC-capable step and
re-reads both from their handles at each use; the work is in a helper so the
single `unpin_native_roots` also covers the element pins on the `?` paths.

## Why one missing registration was worth 67 classes

`org.springframework.test.context.cache.DefaultContextCache.remove()` evicts a
`MergedContextConfiguration` with exactly this call, and
`DefaultCacheAwareContextLoaderDelegate.closeContext()` reaches it on every
`@DirtiesContext`-triggered context close. That is every
`test.context.jdbc.*SqlScriptsTests` class (the largest group), every
`test.context.cache.*` class, and every `@DirtiesContext` user elsewhere in the
suite — `test.context.event.*`, `test.context.hierarchies.*`,
`test.context.bean.override.*`, both `transaction.ejb` families,
`AopNamespaceHandlerAdviceOrderIntegrationTests`,
`AspectJAutoProxy*IntegrationTests`, `messaging.simp.user.*RegistryTests`,
`RequestContextHolderTests`. Two of them
(`ContextHierarchyDirtiesContextTests`, `DirtiesContextEventPublishingTests`)
reported it as `MultipleFailuresError: Test Event Statistics` rather than a bare
NPE, because their failure-aggregating test method wraps what the JUnit
event-recording listener saw.

## The lesson worth keeping

A force-native gate and a native registrar are two lists that must agree, and
**only one of them fails loudly when they do not**. An entry in the gate with no
body behind it is silent by construction — the call runs real JDK bytecode over
a layout that VM does not use, and whether that is visible depends entirely on
whether the class happens to declare the method itself. Six of the seven
carriers hid this for as long as the carriers have existed.

The two regression tests added with the fix
(`set_view_carrier_remove_if_registered`,
`set_view_carrier_mutators_registered`) check the registrar side directly, which
is the side that was empty.

## Repro

```bash
source /data/toolchain/env.sh
cd probes && javac -d /tmp/vrip ViewRemoveIfProbe.java
<cratonvm-bin> --java-home "$JAVA_HOME" -cp /tmp/vrip ViewRemoveIfProbe
$JAVA_HOME/bin/java -cp /tmp/vrip ViewRemoveIfProbe   # the oracle
```

`TOTALFAILS=0` on both is the bar; before the fix CratonVM answered
`TOTALFAILS=3`, all three the `ConcurrentHashMap.entrySet()` rows.

Suite-level:

```bash
cd apps/spring-suite-runner
SPRING=/data/cratonvm/apps/spring-framework JDK25="$JAVA_HOME" \
CRATONVM_BIN=<cratonvm-bin> ./one.sh \
  org.springframework.test.context.cache.LruContextCacheTests
```
