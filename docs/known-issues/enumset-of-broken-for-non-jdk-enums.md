# `EnumSet.of(...)` silently returns an empty/broken set for non-JDK enums

**Status:** OPEN. **Severity:** high — blocks any real-JDK-mode webapp
whose bootstrap path uses `EnumSet.of(...)` on an application/framework
enum (very common pattern; e.g. `jakarta.servlet.DispatcherType` for filter
mapping). **HotSpot:** presumably PASS (this is a CratonVM-only defect).

Found 2026-07-09 while investigating
`docs/known-issues/tomcat-08-07/wsremoteendpoint-close-delay-near-deadlock.md`
— after fixing two other environment blockers (`File.FS` never set, see
`docs/internal/fixed-suite-bugs/file-fs-native-clinit-never-set-FIXED.md`;
`ThreadGroup` native field-index mismatch, see
`docs/internal/fixed-suite-bugs/threadgroup-native-field-index-mismatch-FIXED.md`),
Tomcat startup under real-JDK mode reaches `WsServerContainer`'s
constructor and fails there instead.

## Reproduction

Minimal standalone repro (no Tomcat needed):

```java
import java.util.EnumSet;
import jakarta.servlet.DispatcherType;

public class EnumSetProbe {
    public static void main(String[] args) {
        EnumSet<DispatcherType> types =
            EnumSet.of(DispatcherType.REQUEST, DispatcherType.FORWARD);
        System.out.println("types = " + types);      // "Object@81" (not "[REQUEST, FORWARD]")
        System.out.println("size = " + types.size()); // 0 (should be 2)
        java.util.Iterator<DispatcherType> it = types.iterator();
        System.out.println("iterator = " + it);        // null
        while (it.hasNext()) { }                        // NPE: "Iterator.hasNext()" because "<local2>" is null
    }
}
```

Run with `--java-home <realjdk> -cp "<jakarta-servlet-api jar>:..."`. No
exception is thrown by `EnumSet.of()` itself — it silently returns a
broken/empty set, and the NPE only surfaces later at the first
`iterator()`/for-each use. In Tomcat this happens inside
`org.apache.tomcat.websocket.server.WsServerContainer`'s constructor:

```java
EnumSet<DispatcherType> types = EnumSet.of(DispatcherType.REQUEST, DispatcherType.FORWARD);
fr.addMappingForUrlPatterns(types, true, "/*");   // real Tomcat FilterRegistration for-each's `types`
```

surfacing as:

```
ERROR [org.apache.catalina.core.ContainerBase...] Exception sending context
initialized event to listener instance of class [...Bug66508Config]
(java/lang/NullPointerException: Cannot invoke "java.util.Iterator.hasNext()"
because "<local5>" is null)
```

which fails every `StandardContext` that registers a websocket endpoint
(`WsContextListener` → `WsSci.init` → `new WsServerContainer(...)`).

## Root-cause sketch (not fully confirmed — next session should verify before fixing)

`native-builtins/src/phases_early.rs` implements `EnumSet` as a `SyntheticStub`
(2-field: `elements` = an `ArrayList` backing list, `type` = element class).
`java/util/EnumSet` is one of the classes explicitly listed in
`vm/src/vm/vm_exec.rs`'s `real_protected_stub` set
(`"java/util/EnumSet"` appears alongside `ReentrantLock`,
`LinkedBlockingDeque`, `AtomicBoolean`, `Cleaner`, …) — meaning dispatch
prefers **real bytecode** over the synthetic stub whenever real bytecode
with a `Code` attribute is loaded for the exact (class, method, descriptor).

`EnumSet.of(E, E)` (the `native_es_of_two` native, registered for the exact
2-arg descriptor) first tries `try_jdk_enum_set_of_elements` (in
`phases_early.rs`) — a helper that builds a *real* JDK `EnumSet` via
`invoke("java/util/EnumSet","noneOf",...)` + `invoke_virtual(set,"add",...)`
for each element, returning that real object if all invokes succeed
*without checking that `add()` actually added anything*. Only on any hard
failure (an `Err` from `invoke`) does it fall back to the synthetic 2-field
bridge.

Two candidate explanations for the observed empty-but-non-throwing result,
neither confirmed yet:

1. **Enum-universe lookup fails for non-JDK enums.** Real `EnumSet.noneOf`
   calls `SharedSecrets.getJavaLangAccess().getEnumConstantsShared(class)`,
   which `native_class_get_enum_constants`
   (`native-builtins/src/lang_class.rs`) backs by reading the enum's
   synthetic `$VALUES`/`ENUM$VALUES` static field. If that lookup fails for
   `DispatcherType` (loaded from the webapp/common classloader, not
   bootstrap) — the function already has a `tracing::warn!` for exactly
   this case (`"no $VALUES/ENUM$VALUES field for class={}"` and `"$VALUES
   is null/non-object"`) — real `noneOf` should actually **throw**
   `ClassCastException` in that case (`universe == null` → throw), which
   doesn't match the *silent* empty result observed. Check the warn logs
   for this signature on a fresh repro run first.
2. **Real/synthetic layout mismatch on `size()`/`iterator()` for EnumSet
   *subclasses*.** If `try_jdk_enum_set_of_elements` DOES succeed and
   returns a real `RegularEnumSet`/`JumboEnumSet` instance, later calls to
   `.size()`/`.iterator()` on that instance still have to resolve through
   `invoke_or_native`'s hierarchy walk. If no native is registered
   specifically for `RegularEnumSet`/`JumboEnumSet` (only for the abstract
   `java/util/EnumSet` superclass) and the walk finds and applies the
   *synthetic* `native_es_size`/`native_es_iterator` (which read the
   2-field bridge layout: field 0 = backing `ArrayList`) against a real
   `RegularEnumSet` object (whose field 0 is something else entirely, e.g.
   a `long` bit-vector or `Class` reference) — that would explain a
   silent `size()==0`/`iterator()==null` without any exception. This is
   the more likely explanation given the `Object@81`-style generic
   `toString()` observed (a real `RegularEnumSet.toString()` would print
   `[REQUEST, FORWARD]`; `Object@81` suggests dispatch fell through to
   `Object.toString()`, i.e. neither the real class's own `toString()` nor
   the synthetic native's `toString()` matched).

Recommended next step: add a targeted debug print in
`try_jdk_enum_set_of_elements` (which object/class it actually returns,
and immediately call `size()`/`iterator()` on it right there before
returning) to distinguish these two hypotheses, then either (a) make
`native_class_get_enum_constants` handle non-bootstrap-loaded enums
correctly, or (b) register `size()`/`iterator()`/`toString()`/etc. natives
for the real `RegularEnumSet`/`JumboEnumSet` classes too (or verify real
`add()` actually mutated the set before trusting
`try_jdk_enum_set_of_elements`'s result, falling back to the synthetic
bridge otherwise — cheaper, more defensive fix).

## Impact

Blocks the Tomcat WebSocket close-delay investigation (see the doc linked
at the top) from reaching the actual code under test — `WsServerContainer`
can't construct successfully under real-JDK mode. Likely affects any
other real-JDK-mode suite exercising `EnumSet.of(...)` on a non-JDK enum
during startup (Spring's `EnumSet` usage on JDK enums like
`RoundingMode`/`TimeUnit` is probably fine, since those may go through
different codepaths already covered by existing fixups — but hasn't been
checked).
