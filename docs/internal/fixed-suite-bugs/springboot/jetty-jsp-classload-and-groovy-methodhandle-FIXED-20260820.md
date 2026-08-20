# Jetty's embedded JSP servlet, and Groovy closures as MethodHandles — FIXED 2026-08-20

**Status: FIXED.** Both defects on the 2026-08-19 triage page are root-caused
and closed, along with one residual that only became visible once the second
was out of the way. All 17 classes named below now pass on CratonVM with the
same test counts HotSpot reports on the same fixed classpath and cwd.

| Arm | Jetty cluster (7 classes) | Groovy cluster (10 classes) |
| --- | --- | --- |
| HotSpot control | 7/7 pass, 170 tests, 0 failed | 10/10 pass, 182 tests, 0 failed |
| CratonVM before (`86b13ed4c`, already carrying the §2 fix) | 0/7 pass, **114 test failures** | 7/10 pass, 3 × `containersFailed=1` |
| CratonVM after | 7/7 pass, 170 tests, 0 failed | 10/10 pass, 182 tests, 0 failed |

`JettyServletWebServerFactoryTests` reports `skipped=2` on both VMs, which is
the same two environment-gated methods, not a divergence.

## 1. Jetty's embedded JSP servlet failed to class-load

```
Caused by: jakarta.servlet.UnavailableException: Class loading error for
  holder jsp==org.apache.jasper.servlet.JspServlet@19c47{jsp=null,order=3,
  inst=false,async=true,src=EMBEDDED:<null>,STARTING}
```

### The chain, end to end

`BaseHolder.doStart` logs the real cause one line above the exception, and it
is a bare `ClassNotFoundException` thrown from
`WebAppClassLoader.loadClass(WebAppClassLoader.java:540)` with **no cause and
nothing suppressed**. Line 540 is
`throw ex == null ? new ClassNotFoundException(name) : ex;`, so `ex` was null
— which means the parent loader had *already resolved the class successfully*
and the webapp loader threw it away. There is exactly one branch that does
that: `_context.isHiddenClass(parentClass)` answered `true`.

Spring Boot's `JettyEmbeddedWebAppContext` sets its hidden-class matcher to a
single pattern, `new ClassMatcher("org.springframework.boot.loader.")`, which
cannot possibly hide `org.apache.jasper.servlet.JspServlet`. It could only say
`true` if the matcher were **empty** — and an empty `ClassMatcher` matches
*everything*: `ClassMatcher.combine` short-circuits an empty pattern set into
`locations.test(..)`, and an empty `IncludeExcludeSet` tests vacuously true.

`ClassMatcher` is `AbstractSet<String>` over a private `Map`, with
`iterator()` = `_entries.keySet().iterator()` and `getPatterns()` =
`toArray(new String[size()])`; its copy constructor rebuilds itself from
`getPatterns()`. On CratonVM that array came back **the right length and
entirely null**, so every matcher built from another matcher came out empty.
A 20-line probe with no Jetty on the classpath reproduces it:

```java
Map<String, Integer> m = new LinkedHashMap<>();          // 3 entries
AbstractSet<String> s = new AbstractSet<>() {
    public Iterator<String> iterator() { return m.keySet().iterator(); }
    public int size() { return m.size(); }
};
s.toArray()                  // [x, y, z]        — correct
s.toArray(new String[0])     // [null,null,null] — WRONG
```

### Root cause

`real_jdk_to_array_typed` in `vm/src/vm/vm_init.rs` is the native that services
the inherited `AbstractCollection.toArray(T[])` on the real-JDK path (the JDK's
own bytecode reads `elementData`, which only `ArrayList` has). When the
receiver has no `elementData` it walks the receiver's own `iterator()` — and
that loop dispatched **by name**:

```rust
ctx.invoke("java/util/LinkedHashMap$LinkedKeyIterator", "hasNext", "()Z", ..)
```

A by-name `invoke` resolves the named class's own bytecode and never consults
`should_force_registered_native_over_bytecode`, the gate that exists precisely
because a CratonVM-minted `HashMap$KeyIterator` /
`LinkedHashMap$LinkedKeyIterator` carries its snapshot **past** the fields the
real `HashIterator` bytecode walks (`key_itr_base`). The real `hasNext()`
therefore read an unset `next` field and answered `false` on the *first*
element. Instrumented, the whole defect is two lines:

```
[DBG_TOARRAY] real_jdk_to_array_typed recv=DeclProbe$1 size=3 it_class=java/util/LinkedHashMap$LinkedKeyIterator
[DBG_TOARRAY]   i=0 hasNext=Some(Int(0))
```

The loop broke at `i = 0`, `size()` had already fixed the result length, and
the caller got a right-length array of nulls — the single hardest shape for a
caller to notice, because every length check and null-check-on-the-array
passes.

**Fix** (`d80acdf7b`): dispatch `size` / `iterator` / `hasNext` / `next` with
`invoke_virtual`, which dispatches on the receiver and honours the gate. That
is what every other collection native already uses, and what the zero-arg
`toArray()` over the same receiver was already doing correctly — which is why
`toArray()` was right and `toArray(T[])` was not.

### What the triage page guessed, and how it scored

* The earlier, unlocatable Commons Math write-up the page quoted had the
  mechanism **exactly right** — `isHiddenClass` reading a corrupted pattern set
  from `getPatterns()`, `AbstractCollection.toArray(T[])` on a Map-keySet-backed
  custom `Set` returning a null-content array. What it never had was *which
  code produced the nulls*: the answer is not the JDK bytecode at all, it is
  CratonVM's native replacement for it, which is why re-reading
  `AbstractCollection.toArray` never explained anything.
* The page's "check these two FIXED docs first" hint was a dead end: neither
  `jit-multianewarray-allocated-every-level-with-classid-0` nor
  `interpreter-checkcast-and-instanceof-re-resolved-their-target-every-time`
  touches this path. The defect is not JIT-related at all — it reproduces
  identically under `--nojit`.
* The page's inference that the other six `module/spring-boot-jetty` classes
  shared the root cause was **correct**. All seven were re-verified
  individually; all seven flipped together.

## 2. Groovy closure → `MethodHandle` threw `WrongMethodTypeException`

```
Caused by: java.lang.invoke.WrongMethodTypeException: cannot convert
  MethodHandle(beans,Closure)Object to (Object[])Object
```

Root-caused and fixed on 2026-08-19 in `d766af065`; verified here. Two
independent defects, both reproducible in a 20-line `GroovyShell` probe with
no Spring on the classpath:

1. **`get_or_create_primitive_mirror` wrote `primitive=1` for every name it was
   handed.** It is not only the primitive-mirror factory — it is also the VM's
   generic *stand-in* factory for a class name that could not be resolved to a
   `ClassId`, which a Groovy script class under `GroovyClassLoader$InnerLoader`
   routinely is. `Class.isPrimitive()` is load-bearing in
   `java.lang.invoke`: `MethodTypeForm.canonicalize` erases a reference
   parameter to `Object` only when `!t.isPrimitive()`, so a lying stand-in made
   `findForm` treat an unerased `MethodType` as already erased and die in
   `Wrapper.forPrimitiveType` with `not primitive: beans`. The message is its
   own proof — `Class.toString()` drops the `"class "` prefix exactly when
   `isPrimitive()` is true. `int[].class.isPrimitive()` was wrong for the same
   reason.
2. **`MethodHandle.asSpreader` never installed the adapted `type()`**, so the
   spreader reported its target's *unspread* signature. Groovy's
   `IndyInterface.fallback` does
   `asSpreader(Object[].class, n).asType(methodType(Object.class, Object[].class))`
   on every `invokedynamic` dispatch, and `asType` then correctly refused a
   conversion HotSpot never sees. The refusal was right; the type it was
   reading was wrong.

## 3. The residual the fix uncovered

With (2) fixed, `SpringBootTestGroovyConfigurationTests`,
`SpringBootTestGroovyConventionConfigurationTests` and
`SpringBootTestMixedConfigurationTests` ran every test green and *still*
reported `containersFailed=1`, from a `@DirtiesContext` `afterTestClass`
callback:

```
java.lang.NullPointerException: Cannot invoke
  "java.util.concurrent.ConcurrentHashMap.removeEntryIf(java.util.function.Predicate)"
  because "this.map" is null
    at java.util.concurrent.ConcurrentHashMap$EntrySetView.removeIf(ConcurrentHashMap.java:4856)
    at org.springframework.test.context.cache.DefaultContextCache.remove(DefaultContextCache.java:344)
```

`force_native_over_real_jdk_bytecode` had listed `removeIf` for the
keySet/entrySet carriers since those gates were written, and
`register_set_view_carrier_natives` registered it on **none** of them. *A gate
entry is not a registration*: the lookup found nothing to prefer and fell
through to exactly the bytecode the gate exists to avoid. Five of the six
carriers survived that on luck — the JDK does not override
`Collection.removeIf` on them, so the iterator-based default ran. The sixth,
`ConcurrentHashMap$EntrySetView`, *does* declare it, as
`return map.removeEntryIf(filter)`, over a `map` field a CratonVM-minted view
never fills.

**Fix** (`8c350a300`): `native_hs_remove_if`, registered on the whole family,
deleting through `native_hs_remove` so a removal reaches the backing map.

## Regression tests

Two new integration tests, both self-checking (no HotSpot arm, and neither can
pass vacuously — a VM that iterates nothing reports mismatched lengths rather
than agreement):

* `vm/tests/abstract_collection_to_array_typed.rs` →
  `cratonvm/MapBackedSetToArrayProbe` — every `toArray` overload against the
  receiver's own iterator, over a `LinkedHashMap` keySet, a `HashMap` keySet
  and a `values()` view. 6 rows fail on the pre-fix binary, 0 after. A
  `--nojit` arm rules a compiled body in or out.
* `vm/tests/map_view_remove_if.rs` → `cratonvm/MapViewRemoveIfProbe` —
  `entrySet`/`keySet`/`values` `removeIf` over ConcurrentHashMap, HashMap,
  LinkedHashMap, TreeMap and Hashtable, asserting on the **backing map** each
  time so a view that "removed" from a detached snapshot fails too, plus the
  no-match case (must return `false`, change nothing). The pre-fix binary dies
  on the first row.

## Verification

One process per class, launched exactly as
`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1` does (module root as
cwd, `--add-opens=java.base/java.net=ALL-UNNAMED`, `CRATONVM_REAL=net-sockets,aqs`,
`CRATONVM_THREADS=-default-watchdog`, `CRATONVM_JIT=rootsnap-cache`, `-Xmx 2g`),
on the Azure Linux host against JDK 25. Three CratonVM arms — before, after the
`toArray` fix, after both fixes — plus a HotSpot control over the identical
class list.

The before/after arms are the same 17 classes on the same fixture tree, so the
comparison is a flip of the same rows and not a count delta between two
differently-composed runs.

The Jetty cluster:

| Class | before | after |
| --- | --- | --- |
| `AutoConfigureWebServerJettyServletTests` | 1/1 failed | 1 tests, 0 failed |
| `metrics.JettyMetricsAutoConfigurationTests` | 7/11 failed | 11 tests, 0 failed |
| `autoconfigure.servlet.JettyServletWebServerAutoConfigurationTests` | 7/12 failed | 12 tests, 0 failed |
| `servlet.JettyServletWebServerFactoryTests` | 86/116 failed | 116 tests, 0 failed |
| `servlet.JettyServletWebServerMvcIntegrationTests` | 2/2 failed | 2 tests, 0 failed |
| `autoconfigure.servlet.JettyServletWebServerServletContextListenerTests` | 2/2 failed | 2 tests, 0 failed |
| `autoconfigure.JettyWebServerFactoryCustomizerTests` | 9/26 failed | 26 tests, 0 failed |

The Groovy cluster — `BeanDefinitionLoaderTests` (13),
`SpringBootTestGroovy{,Convention}ConfigurationTests` (1 each),
`SpringBootTestMixedConfigurationTests` (1), `CacheAutoConfigurationTests`
(59), `GroovyTemplateAutoConfigurationTests` (27),
`HazelcastAutoConfiguration{Client,Server}Tests` (12, 20),
`Thymeleaf{Reactive,Servlet}AutoConfigurationTests` (21, 27) — is 10/10 clean,
where the three `core/spring-boot-test` classes were `containersFailed=1`
before the residual fix.

## Notes for whoever reads this next

* **A by-name `ctx.invoke` from a native is not a virtual call.** It resolves
  the named class's own bytecode and skips the force-native gate. Any native
  that drives a JDK object CratonVM mints itself must use `invoke_virtual`.
  One deliberate exception survives, in `native_snapshot_itr_has_next`, which wants
  the real bytecode and guards its own re-entry.
* **A gate entry is not a registration.** Both defects here are the same shape
  seen from two sides: machinery that says "prefer the native" over a class
  whose real bytecode cannot work, with no native actually installed for the
  method in question.
* **A right-length array of nulls is the worst failure mode in this family.**
  Nothing downstream length-checks its way out of it, and the symptom surfaces
  arbitrarily far away — here, as a class-loading error for a class that had
  already been resolved.

## Related

* The 2026-08-19 3-GC sweep this page's two defects were extracted from
  reported 93/90/91 FAILs against a 39-FAIL baseline. Most of that increase was
  two harness confounds (a driver that never `cd`'d into the module directory,
  breaking every relative-path fixture; a missing
  `--add-opens=java.base/java.net`), not CratonVM. The `RESULTS-20260819-3gc-azure`
  and `RESULTS-20260819-3gc-azure-CORRECTION` files the triage page cited were
  never committed and no longer exist anywhere; the accounting above is the
  surviving record.
* `docs/known-issues/springboot/` no longer carries this page.
