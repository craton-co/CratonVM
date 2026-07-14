# `Map.Entry::getKey`/`getValue` method references over synthetic wrapper entries: interface-level native override wins over the wrapper's own, returning the wrong object

**Status: OPEN**

## Symptom

5 Spring Boot structured-logging test classes in `core/spring-boot` all fail
with the identical `ClassCastException`, always inside
`org.springframework.boot.logging.structured.ContextPairs$Pairs`:

```
org.springframework.boot.logging.structured.ElasticCommonSchemaStructuredLogFormatterTests
org.springframework.boot.logging.structured.GraylogExtendedLogFormatStructuredLogFormatterTests
org.springframework.boot.logging.structured.LogstashStructuredLogFormatterTests
org.springframework.boot.logging.structured.ContextPairsTests
org.springframework.boot.logging.structured.StructuredLoggingJsonPropertiesJsonMembersCustomizerTests
```

Example (`ContextPairsTests`, `shard2/logs/core_spring-boot.org.springframework.boot.logging.structured.ContextPairsTests.out.log`):

```
JUnit Jupiter:ContextPairsTests:flatIncludesName()
  => java.lang.ClassCastException: java.util.AbstractMap$SimpleEntry cannot be cast to java.lang.String
     org.springframework.boot.logging.structured.ContextPairs$Pairs.lambda$add$1(ContextPairs.java:168)
     org.springframework.boot.logging.structured.ContextPairs$Pairs.lambda$add$0(ContextPairs.java:167)
     org.springframework.boot.logging.structured.ContextPairs$Pairs.lambda$flat$0(ContextPairs.java:188)
     org.springframework.boot.logging.structured.ContextPairs$Pairs.flat(ContextPairs.java:188)
     org.springframework.boot.logging.structured.ContextPairsTests.apply(ContextPairsTests.java:181)
     org.springframework.boot.logging.structured.ContextPairsTests.flatIncludesName(ContextPairsTests.java:48)
```

The `LogstashStructuredLogFormatterTests` failure shows the same throw
reached through the real JSON-writing pipeline (not just the unit test
harness), confirming this is not a test-only artifact:

```
=> java.lang.ClassCastException: java.util.AbstractMap$SimpleEntry cannot be cast to java.lang.String
   org.springframework.boot.logging.structured.ContextPairs$Pairs.lambda$add$1(ContextPairs.java:168)
   org.springframework.boot.logging.structured.ContextPairs$Pairs.lambda$add$0(ContextPairs.java:167)
   org.springframework.boot.logging.structured.ContextPairs$Pairs.lambda$flat$0(ContextPairs.java:188)
   org.springframework.boot.logging.structured.ContextPairs$Pairs.flat(ContextPairs.java:188)
   org.springframework.boot.json.JsonWriter$Member.lambda$getWritableJsonToWrite$1(JsonWriter.java:665)
   org.springframework.boot.json.JsonValueWriter.writePairs(JsonValueWriter.java:244)
   ...
   org.springframework.boot.logging.structured.JsonWriterStructuredLogFormatter.format(JsonWriterStructuredLogFormatter.java:73)
   org.springframework.boot.logging.logback.LogstashStructuredLogFormatterTests.shouldFormat(LogstashStructuredLogFormatterTests.java:68)
```

All 5 classes throw at the exact same two source lines
(`ContextPairs.java:167`/`168`), only differing in the caller above `flat`/
`nested`. Every affected test constructs its context map with
`Map.of("spring", "boot")` (or similar) and iterates it through
`ContextPairs$Pairs.addMapEntries`, per spring-boot's own
`ContextPairsTests.java`:

```java
Map<String, String> map = Map.of("spring", "boot");
Map<String, Object> actual = apply(contextPairs.flat(".", (pairs) -> pairs.addMapEntries((item) -> map)));
```

`ContextPairs$Pairs.addMapEntries` (spring-boot's
`core/spring-boot/src/main/java/org/springframework/boot/logging/structured/ContextPairs.java`)
is:

```java
public <V> void addMapEntries(Function<T, Map<String, V>> extractor) {
    add(extractor.andThen(Map::entrySet), Map.Entry::getKey, Map.Entry::getValue);
}
```

`add`'s per-element body (the code at lines 167/168) is:

```java
elements.forEach((element) -> {
    String name = nameExtractor.apply(element);   // line 168 — nameExtractor = Map.Entry::getKey
    V value = valueExtractor.apply(element);
    pairs.accept(name, value);
});
```

So `nameExtractor.apply(element)` — `Map.Entry::getKey` invoked as an
**unbound instance method reference** through `Function<Entry,String>` —
returns the whole `Map.Entry` instead of the key, and the subsequent
implicit `checkcast String` throws.

## Root cause — confirmed and isolated

Reproduced standalone (no Spring Boot involved), isolating the bug to
`Map.Entry::getKey`/`getValue` **method references specifically**, not
direct `entry.getKey()` calls:

```java
import java.util.Map;
import java.util.function.Function;

public class EntryRepro {
    public static void main(String[] args) {
        Map<String, String> map = Map.of("spring", "boot");
        Function<Map.Entry<String, String>, String> nameExtractor = Map.Entry::getKey;
        for (Map.Entry<String, String> e : map.entrySet()) {
            System.out.println("entry class=" + e.getClass());   // cratonvm.internal.UnmodifiableMapEntry
            String name = nameExtractor.apply(e);                 // throws here
        }
    }
}
```

```
entry class=class cratonvm.internal.UnmodifiableMapEntry
Exception in thread "main" java/lang/ClassCastException: java.util.AbstractMap$SimpleEntry cannot be cast to java.lang.String
	at EntryRepro.main(EntryRepro.java:10)
```

A sibling repro that calls `e.getKey()` **directly** (ordinary
`invokeinterface`, no method reference/lambda involved) on the exact same
`Map.of(...)` entry works correctly and returns `"spring"`:

```java
Map<String, String> map = Map.of("spring", "boot");
for (Map.Entry<String, String> e : map.entrySet()) {
    Object k = e.getKey();          // "spring" — correct
}
```

This isolates the bug to the **method-reference/lambda dispatch path**
specifically (`vm/src/runtime/interpreter.rs::try_lambda_dispatch`), not to
`Map.of()`'s entry construction or `getKey`'s native implementation in
general — both of which are fine, as the direct-call repro proves.

### Why: two competing native overrides for the same abstract interface method

`Map.of("spring", "boot").entrySet()` is CratonVM's synthetic immutable-map
path (`native-builtins/src/phases_early.rs:1524-1534`, `Map.of(K,V)`
allocates a synthetic `HashMap` and populates it via
`native_map_put_pub`). Its `entrySet()` view wraps each live entry for
read-only iteration in `cratonvm/internal/UnmodifiableMapEntry`
(`native-collections/src/lib.rs:34141-34148`,
`native_unmod_entry_itr_next`):

```rust
fn native_unmod_entry_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let next = unmod_delegate(ctx, args, "next", "()Ljava/lang/Object;")?;
    if let Some(Value::Object(Some(entry))) = next {
        let w = alloc_unmod_wrapper(ctx, UNMOD_MAP_ENTRY_CLASS, entry);   // wrapper: field 0 = backing entry
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(next)
}
```

`UnmodifiableMapEntry`'s field 0 is documented as the **backing (real)
entry**, not the key (`native-collections/src/lib.rs:32931-32932`,
`UNMOD_FIELD_BACKING = 0`). Its `getKey` is registered
(`native-collections/src/lib.rs:33892-33894`) to correctly *delegate*
rather than read field 0 directly:

```rust
r.register(c, "getKey", "()Ljava/lang/Object;", |ctx, args| {
    unmod_delegate(ctx, args, "getKey", "()Ljava/lang/Object;")   // forwards to the real backing entry's getKey()
});
```

But there is **also** a generic native registered directly on the
*interface* `java/util/Map$Entry` (`native-collections/src/lib.rs:18523-18528`),
used as a fallback for entries built with class name `java/util/Map$Entry`
directly (2/3-field layout, key at slot 0):

```rust
registry.register("java/util/Map$Entry", "getKey", "()Ljava/lang/Object;", native_entry_get_key);
// fn native_entry_get_key(...) { ... Ok(Some(ctx.get_field(this, 0))) }
```

`UnmodifiableMapEntry` is a purely synthetic class with **no bytecode
methods of its own** — it only records `implements Map$Entry`
(`vm/src/vm/vm_init.rs:1218`). So when `Map.Entry::getKey` is dispatched
as a **method reference** (`try_lambda_dispatch`, `InvokeInterface` kind,
`vm/src/runtime/interpreter.rs:20084-20263`), the receiver class name used
for virtual dispatch resolves through `invoke_on_class_shared` →
`invoke_on_class_shared_inner` (`vm/src/vm/vm_exec.rs:12039`), whose
`find_method_recursive(class_id, "getKey", ...)` walk finds **no** concrete
`getKey` on `UnmodifiableMapEntry` itself and returns the *abstract*
declaration on `java/util/Map$Entry` instead — `declaring_id` names the
**interface**, not the concrete receiver class.

The subsequent native-override check
(`vm/src/vm/vm_exec.rs:14512-14519`) looks up
`shared.native_methods.find(class_name, "getKey", descriptor)` using that
interface's class name (`"java/util/Map$Entry"`) — which **matches**
(`native_entry_get_key`, the generic fallback) and immediately sets
`native = true`. Because `native` is already `true`, the later, more
specific "C25 rescue" block
(`vm/src/vm/vm_exec.rs:14526-14537`, comment: *"natives for synthetic
wrapper classes are registered on the wrapper class name, not the
interface... without this, dispatch on an `Enumeration$Impl` receiver...
resolves to the abstract method and fails"*) — which is exactly the
mechanism that would have found and preferred
`UnmodifiableMapEntry.getKey` (the correct, delegating override) — is
**short-circuited by the `!native` guard** and never runs.

The net effect: `Map.Entry::getKey` invoked as a method reference on an
`UnmodifiableMapEntry` receiver runs `native_entry_get_key`'s
`ctx.get_field(this, 0)` directly on the **wrapper**, not on its backing
entry. Field 0 of the wrapper is `UNMOD_FIELD_BACKING` — the real
`AbstractMap$SimpleEntry`-backed entry object — so `getKey()` returns that
whole entry object instead of its key string. Spring Boot's
`(String) nameExtractor.apply(element)` then throws exactly the observed
`ClassCastException: java.util.AbstractMap$SimpleEntry cannot be cast to
java.lang.String`.

Ordinary `invokeinterface` bytecode (`e.getKey()`, no method reference) does
not hit this: it resolves through a different call site
(`invoke_or_native`/direct interpreter dispatch on the receiver's exact
concrete class name) that correctly finds `UnmodifiableMapEntry`'s own
`getKey` registration first — confirmed by the direct-call repro above
succeeding.

**This is a genuine CratonVM native-dispatch precedence bug**: a generic
interface-level native override (registered under the interface's own
name, intended as a fallback for entries built directly as
`java/util/Map$Entry`) incorrectly wins over a more specific
wrapper-class-level native override, specifically on the
method-reference/lambda dispatch path, because that path's native-override
lookup uses the *abstract method's declaring interface name* instead of
the *receiver's actual concrete class name*, and the existing "C25 rescue"
that handles this exact class of problem elsewhere
(`vm/src/vm/vm_exec.rs:14520-14537`) is gated by `!native`, so it never
runs once the interface-level lookup already (wrongly) succeeded. This is
not a Spring Boot or JDK issue — `Map.of()`'s entries and `Map.Entry::getKey`
are used exactly as intended; it is not related to the same-day
HashMap/Integer native-dispatch perf commit (`77f8b37e5`), which does not
touch `Map.Entry`/`entrySet`/method-reference dispatch at all.

Likely affects any synthetic wrapper class (not just
`UnmodifiableMapEntry`) that (a) implements an interface with a generic,
name-registered fallback native and (b) is itself invoked only via a
method reference rather than direct `invokeinterface` bytecode — the
`Enumeration$Impl`/`Iterator.hasNext()` case the C25 comment references
is presumably fine only because that specific combination is never reached
through a method reference in the tests exercised so far.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Category all -Jit on -Parallel 1 -RunName mapentry-getkey-lambda-repro-20260714 `
  -SpringBootRoot C:\craton\CratonVM-spring-boot-crashfail-20260714\apps\spring-boot
```

Then use `-ListOnly` against the generated `all-tests.tsv`
(per `apps\spring-boot-suite-runner\run-spring-boot-suite.md`) to find the
row indices, or run a `-Start`/`-Count` slice covering:

- `core/spring-boot` → `org.springframework.boot.logging.structured.ContextPairsTests`
- `core/spring-boot` → `org.springframework.boot.logging.structured.StructuredLoggingJsonPropertiesJsonMembersCustomizerTests`
- `core/spring-boot` → `org.springframework.boot.logging.logback.ElasticCommonSchemaStructuredLogFormatterTests`
- `core/spring-boot` → `org.springframework.boot.logging.logback.GraylogExtendedLogFormatStructuredLogFormatterTests`
- `core/spring-boot` → `org.springframework.boot.logging.logback.LogstashStructuredLogFormatterTests`

Minimal standalone reproduction (no Spring Boot / suite runner needed),
compiled with a real JDK `javac` and run against
`target\release\cratonvm.exe`:

```java
import java.util.Map;
import java.util.function.Function;

public class EntryRepro {
    public static void main(String[] args) {
        Map<String, String> map = Map.of("spring", "boot");
        Function<Map.Entry<String, String>, String> nameExtractor = Map.Entry::getKey;
        for (Map.Entry<String, String> e : map.entrySet()) {
            System.out.println(nameExtractor.apply(e));   // throws ClassCastException
        }
    }
}
```

```powershell
javac EntryRepro.java
target\release\cratonvm.exe -c . EntryRepro
```

Confirmed on this worktree's release build (`target\release\cratonvm.exe`,
branch `feat/spring-boot-crashfail-20260714`, at the tip of `dev` as of
2026-07-14, commit `1021533f9`).

## Related

- `docs/known-issues/springboot/comparable-classcast-lambda-proxy-unknown-class.md`
  — a different bug in the same neighborhood (native-dispatch gaps
  surfacing specifically through method-reference/lambda call sites), but
  a distinct root cause: that doc is about **lambda-proxy classes** not
  being registered in `class_manager` at all (`Collections.sort`'s
  `Comparable` check on a lambda receiver); this doc's receiver
  (`UnmodifiableMapEntry`) *is* a normal `class_manager`-registered
  synthetic class — the bug here is a **native-override precedence**
  ordering mistake (interface-level generic native beating a more specific
  wrapper-class native), not a missing-registry lookup.
- `reference_descriptor_coercion_slot_reuse_trap` /
  `reference_overlay_real_class_corruption` (session memory) — **not** this
  family: no field-count ambiguity or corrupted object header is involved;
  the wrapper's field 0 is exactly what it is documented to be
  (`UNMOD_FIELD_BACKING`), and the bug is purely about *which native
  function* answers `getKey()`, not about misreading a field.
- `vm/src/vm/vm_exec.rs:14520-14537` — the existing "C25 rescue" comment
  block that already solves this exact class of problem for
  `Enumeration$Impl`/`Iterator.hasNext()`-shaped cases; it needs to also
  fire when a *more specific* native match exists on the receiver's own
  class even after a *less specific* interface-level native already
  matched, not just when no native matched at all.
