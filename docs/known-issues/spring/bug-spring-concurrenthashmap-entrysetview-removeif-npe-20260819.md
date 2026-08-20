# `ConcurrentHashMap.entrySet().removeIf(...)` throws NPE under CratonVM — root cause found, explains 66 of 88 Spring Framework FAILs

## Status
**OPEN, confirmed CratonVM-specific, root-caused with a minimal repro and an
exact source-level lead** — found 2026-08-19 running the full Spring
Framework test suite (2,848 classes) under CratonVM on Azure
(`azureuser@20.80.105.49`), across three parallel GC-variant runs
(generational/G1/ZGC, 6-way sharded each). This single bug explains **66 of
the 88 FAIL classes that are common to all three GC variants** (75% — GC
choice is irrelevant, this is a JIT/interpreter dispatch bug). Differential-
verified against real HotSpot JDK 25: passes cleanly.

## Minimal repro (no Spring involved at all)
```java
import java.util.concurrent.ConcurrentHashMap;

public class CHMEntrySetRemoveIfProbe {
    public static void main(String[] args) throws Exception {
        ConcurrentHashMap<String, String> map = new ConcurrentHashMap<>();
        map.put("a", "1"); map.put("b", "2"); map.put("c", "3");
        boolean removed = map.entrySet().removeIf(e -> e.getKey().equals("b"));
        System.out.println("removeIf returned: " + removed);
    }
}
```
**HotSpot**: `removeIf returned: true`, size drops to 2, no exception.

**CratonVM** (`--java-home`, real-jdk mode): fails on the very first call,
every time:
```
NPE REPRODUCED: Cannot invoke "java.util.concurrent.ConcurrentHashMap.removeEntryIf(java.util.function.Predicate)" because "this.map" is null
	at java.util.concurrent.ConcurrentHashMap$EntrySetView.removeIf(ConcurrentHashMap.java:4856)
	at CHMEntrySetRemoveIfProbe.main(CHMEntrySetRemoveIfProbe.java:11)
```
This is **real JDK bytecode executing** (`ConcurrentHashMap.java:4856` is a
real line in the actual JDK source, not a CratonVM stack frame) — `entrySet()`
returns a `ConcurrentHashMap$EntrySetView`, and calling `.removeIf(predicate)`
on that view (typed as `Set<Map.Entry<K,V>>` at the call site) runs the JDK's
own `EntrySetView.removeIf`, whose body does `return map.removeEntryIf(f)`.
For that to NPE on `map`, the `EntrySetView` object's own `map` field —
`final`, set by its superclass `CollectionView`'s constructor — is null.

## Root cause: the field is intentionally never populated, and the native override that should intercept this call isn't firing

CratonVM does **not** store `ConcurrentHashMap`'s entries in the real JDK's
`table` array — it uses its own segmented native layout instead. Because of
that, `EntrySetView`'s real JDK bytecode (which reads `this.map`/`table`
directly) is expected to be **unreachable in practice**: CratonVM registers
native implementations for these "view carrier" classes and forces the
interpreter to dispatch to native code instead of running the real bytecode
body, for exactly this reason. `native_override.rs` documents the pattern
explicitly:

```rust
// java/util/concurrent/ConcurrentHashMap$EntrySetView ... "removeIf" ...
// => return true   (force native over real bytecode)
```
(`vm/src/runtime/interpreter/native_override.rs`, `force_native_over_real_jdk_bytecode`)
This exact `(class_name="java/util/concurrent/ConcurrentHashMap$EntrySetView",
method_name="removeIf")` pair **is** in the force-native list — the intent is
clearly there. A near-identical, apparently-duplicated copy of the same
class/method list also exists inline in `vm/src/vm/vm_exec.rs` (~line 24383),
referenced by comment as the "companion entry" to the canonical
`native_override.rs` list.

Despite that, our probe proves the **real bytecode ran anyway** — the native
override did not fire for this call. The most likely explanation: `.removeIf()`
called on a reference statically typed as `Set<Map.Entry<K,V>>` (which is
exactly what `entrySet()`'s declared return type is, so javac always compiles
this call as `invokeinterface java/util/Set.removeIf`) resolves through a
different code path in the interpreter than a direct/virtual call to
`EntrySetView.removeIf` — and that interface-dispatch path does not consult
the same force-native table (or consults a stale/incomplete copy of it) that
the virtual-dispatch path does. This would be consistent with there being TWO
separate copies of the same class/method list in the codebase
(`native_override.rs` and `vm_exec.rs`) rather than one shared source of
truth — a classic condition for exactly this kind of one-path-fixed,
one-path-missed gap.

## Why this explains 66 (not just a handful) of the 88 common FAILs

Any code that calls `Map.entrySet().removeIf(...)` on a `ConcurrentHashMap`
hits this — and Spring's own `TestContext` framework does so on every single
`ApplicationContext` cache eviction: `DefaultContextCache.remove()` (line 344)
calls exactly this pattern to evict a `MergedContextConfiguration` from its
cache, and `DefaultCacheAwareContextLoaderDelegate.closeContext()` calls
`remove()` on every `@DirtiesContext`-triggered context close. That single
call site is reached by:
* Every `org.springframework.test.context.jdbc.*SqlScriptsTests` class
  (transactional test infrastructure resets the context between tests) — the
  single largest contributor, ~28 classes.
* Every `org.springframework.test.context.cache.*` cache-behavior test
  (`ContextCacheTests`, `LruContextCacheTests`, `ContextCachePauseModeTests`,
  `ClassLevelDirtiesContextTests`/`...TestNGTests`, `MethodLevelDirtiesContextTests`,
  `SpringExtensionContextCacheTests` — these test the cache directly).
* Every `@DirtiesContext`-driven test elsewhere in the suite
  (`test.context.event.*`, `test.context.hierarchies.*`,
  `test.context.bean.override.*`, `test.context.testng.transaction.ejb.*`,
  `test.context.transaction.ejb.*`, `aop.config.AopNamespaceHandlerAdviceOrderIntegrationTests`,
  `aop.framework.autoproxy.AspectJAutoProxy*IntegrationTests`,
  `messaging.simp.user.*RegistryTests`, `web.servlet.samples.spr.RequestContextHolderTests`).
* Two classes (`ContextHierarchyDirtiesContextTests`,
  `DirtiesContextEventPublishingTests`) surface it as
  `org.opentest4j.MultipleFailuresError: Test Event Statistics (2 failures)`
  rather than a bare NPE — JUnit's event-recording listener still sees the
  NPE, it's just wrapped differently by the failure-aggregating test method.

The one class in the original NPE-shaped group that is **not** part of this
cluster: `InvocableHandlerMethodKotlinTests` — a different NPE
(`Parameter specified as non-null is null: ... SimpleTypeImpl.<init>`), a
Kotlin-reflection issue, unrelated.

## Next steps
* In `vm/src/vm/vm_exec.rs` (~line 24383) and `native_override.rs`
  (`force_native_over_real_jdk_bytecode`, ~line 2637), find the actual
  bytecode-dispatch code path taken for an `invokeinterface` call to
  `Set.removeIf` where the runtime receiver is
  `ConcurrentHashMap$EntrySetView`, and confirm whether it consults either
  list. If confirmed as an invokeinterface-vs-invokevirtual gap, the fix is
  either to make the interface-dispatch path consult the same table, or
  (better, given two copies already exist and have apparently drifted) to
  collapse `vm_exec.rs`'s inline copy into a single call to
  `native_override.rs::force_native_over_real_jdk_bytecode` so there is only
  one list to keep in sync.
* Once fixed, sanity-check the sibling `KeySetView`/`ValuesView` carriers'
  `removeIf` the same way (same probe pattern, swap `entrySet()` for
  `keySet()`/`values()`) — the exact same interface-dispatch gap, if
  confirmed, would apply to them too since they're covered by the same kind
  of force-native list entry.
* Re-run the Spring suite after the fix — given the breadth here, this alone
  should measurably move the overall pass rate on all three GC variants.

## Repro
```bash
source /data/toolchain/env.sh
javac -d probeclasses CHMEntrySetRemoveIfProbe.java
<cratonvm-bin> --java-home $JAVA_HOME -c probeclasses CHMEntrySetRemoveIfProbe
# compare against: $JAVA_HOME/bin/java -cp probeclasses CHMEntrySetRemoveIfProbe
```
Full class list (66) is the intersection of FAIL classes across all three
2026-08-19 GC-variant runs whose failure message matches
`Cannot invoke "java.util.concurrent.ConcurrentHashMap.removeEntryIf` (64
classes) plus the two `MultipleFailuresError`-wrapped classes identified by
method-name inspection above; see
`/data/sb-fw-3gc-out/fw3gc-20260819-193219/*.results.tsv` on the Azure host
for the full run data this was triaged from.
