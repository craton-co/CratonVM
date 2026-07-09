# `EnumSet.of(...)` silently returns an empty/broken set for non-JDK enums

**Status:** OPEN. **Severity:** critical — confirmed 2026-07-09 (see the ES
update at the bottom) to also break `EnumSet.allOf(Class)`, and to be the
**dominant blocker for the entire real-JDK-mode Elasticsearch suite**: every
class hits it via `org.apache.logging.log4j.Level.<clinit>` at logging
bootstrap (2646/2648 non-passed classes FAIL in a fresh full-suite rerun,
essentially all through this one gap). Originally scoped as "blocks any
real-JDK-mode webapp whose bootstrap path uses `EnumSet.of(...)` on an
application/framework enum" (e.g. `jakarta.servlet.DispatcherType` for
filter mapping) — that undersold it. **HotSpot:** presumably PASS (this is
a CratonVM-only defect).

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

## 2026-07-09 update: also breaks `allOf`, confirmed as the ES-suite-wide blocker

Found independently while verifying
`docs/known-issues/elasticsearch-suite/ES-RUN-20260709-currentdev-nonpassed-rerun-120s-summary.md`
after fixing two other masking bugs (real-JDK `EnumMap.<init>` corruption
and reversed `StackWalker` frame order — see
`docs/internal/fixed-suite-bugs/enummap-realmode-corruption-and-stackwalker-frame-order-FIXED.md`).
A fresh full rerun of the 2649-class ES non-passed selection against a
`dev` build with both of those fixed shows 2646/2648 rows FAIL, almost all
through this bug:

```
java.lang.NullPointerException: Cannot invoke "java.util.Iterator.hasNext()" because "<local2>" is null
	at org/apache/logging/log4j/spi/StandardLevel.getStandardLevel(StandardLevel.java:91)
	at org/apache/logging/log4j/Level.<init>(Level.java:145)
	at org/apache/logging/log4j/Level.<clinit>(Level.java:86)
	at org/elasticsearch/common/logging/LogConfigurator.<clinit>(LogConfigurator.java:76)
```

`org.apache.logging.log4j.spi.StandardLevel` (log4j-api, not bootstrap
classloader — a non-JDK enum, same shape as the `DispatcherType` repro
above) evidently builds/consumes an `EnumSet` here, and every ES test
class initializes logging, so this single call site blocks virtually the
whole suite.

Confirms **Hypothesis 2 exactly**, and extends it: `EnumSet.allOf(Class)`
is equally broken, not just `of(...)`. Minimal repro:

```java
import java.util.EnumSet;
public class Repro {
    enum Color { RED, GREEN, BLUE }
    public static void main(String[] a) {
        EnumSet<Color> all = EnumSet.allOf(Color.class);
        System.out.println(all);           // Object@55 (not "[RED, GREEN, BLUE]")
        System.out.println(all.size());     // 0 (should be 3)
        System.out.println(all.iterator()); // null
    }
}
```

Note `EnumSet.allOf`/`noneOf` are *both* registered (in
`native-builtins/src/phases_early.rs::register_enum_set_natives_with_category`)
to the same `native_es_none_of` — which ignores its `Class` argument and
always builds an empty synthetic 2-field bridge — so `allOf`'s brokenness
may not even need the `try_jdk_enum_set_of_elements` real-object path
implicated in Hypothesis 2 for `of(...)`; it could be failing for a
simpler reason (always-empty by construction) that then hits the *same*
downstream `size()`/`iterator()`/`toString()` dispatch confusion once
something (real bytecode?) expects a non-empty real object. Worth checking
both paths converge on the same root cause before fixing only one.

Not fixed this session — flagging severity/scope only, per the note at
the top. Recommended starting point is still the debug-print step already
suggested above, now on the `allOf`/`Level.<clinit>` repro (a much shorter,
non-Tomcat repro path) rather than the original Tomcat one.
