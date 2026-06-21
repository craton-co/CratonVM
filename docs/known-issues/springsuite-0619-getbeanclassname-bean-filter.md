<!-- One file per CratonVM-unique crash/hang/correctness root cause. -->
# bug-B: `getBeanClassName()` native filter hides loadable bean classes → null bean → "Target object must not be null"

| | |
|---|---|
| **Category** | VM-CORRECTNESS (Spring bootstrap intervention) |
| **Module** | spring-beans, spring-aop (+ context) |
| **Test class(es)** | `beans.factory.support.LookupMethodTests`, `…FactoryBeanTests`, `…annotation.LookupAnnotationTests`, `aop.aspectj.*`, `aop.target.CommonsPool2TargetSourceProxyTests`, … (**~24 classes**) |
| **Failing test(s)** | every method that instantiates the affected bean (e.g. `LookupMethodTests` 0/7) |
| **CratonVM** | FAIL — `IllegalArgumentException: Target object must not be null` |
| **HotSpot JDK 25** | OK (LookupMethodTests 7/7; instance is `…AbstractBean$$SpringCGLIB$$0`) |
| **CratonVM HEAD** | found `c4536b94`; primary **FIXED on dev `3b16e985`+**; bug-B2 **mostly FIXED on dev 2026-06-21** |
| **Status** | 🟢 **MOSTLY FIXED** — primary bean-filter FIXED; bug-B2 method-injection implemented; **generic-type disambiguation FIXED on dev `a67ec290`** (`LookupMethodTests` **7/7**, `LookupAnnotationTests` **9/10**). One narrow residual: `@Lookup` null-bean (`withNullBean`, 1 test). |
| **Suggested owner** | residual: `withNullBean` — overload-aware override mapping + `NullBean`→`null` unwrap |

> **GENERIC-TYPE FIX LANDED 2026-06-21** (dev `a67ec290`, JDK 25). `build_lookup_subclass`'s
> by-type, no-arg path now emits Spring's generic-aware resolution —
> `getBeanProvider(ResolvableType.forMethodReturnType(getClass().getSuperclass().getDeclaredMethod(name))).getObject()`
> — instead of `getBean(Ret.class)`, so `NumberStore<Double>` vs `NumberStore<Float>` disambiguate.
> **`LookupMethodTests` 6/7 → 7/7; `LookupAnnotationTests` 6/10 → 9/10.** Verified vs HotSpot via a
> JUnit-platform runner over `spring-beans/build/cratonvm-testcp.txt`: stable across 4 JIT runs **and**
> under `--nojit` (7/7, no hang). **The two blockers from the earlier reverted attempt no longer
> reproduce** — the flaky JIT `InterceptingExecutableInvoker.invokeVoid` "expected int got ref" and the
> `--nojit` hang were cleared by intervening JNI/JIT merge fixes. Remaining residual = `withNullBean`
> (by-name `@Lookup("testBean")` to a null-producing prototype): needs (a) overload-aware override
> mapping so `get()` is recognised by-name despite the overloaded by-type `get(String)` (the override
> map is keyed by method name only), and (b) a `bean.equals(null) ? null : bean` NullBean unwrap on the
> by-name path.

> **bug-B2 FIX 2026-06-21** (`cratonvm-spring0620-b2`, dev `7c66d89f`+, JDK 25). Method-injection
> (`<lookup-method>` / `@Lookup`) is now implemented. The bean class is abstract, so the
> `SimpleInstantiationStrategy.instantiate` shim
> (`s_instantiation_strategy_instantiate` → `try_build_method_injection`) synthesises a concrete
> CGLIB-style subclass via `cglib_enhancer::build_lookup_subclass`: a `$$beanFactory` field + an
> override per abstract method that returns `(Ret) bf.getBean(name|Ret.class[, boxedArgs])`
> (non-lookup abstract methods get a throwing stub so the subclass stays concrete). The owning
> factory is stored into `$$beanFactory` right after `new_object`. Results:
> **`LookupMethodTests` 0/7 → 6/7**, **`LookupAnnotationTests` 0/10 → 6/10** (no more "Target
> object must not be null").
>
> **Residual (OPEN, narrow):**
> 1. **Generic-type disambiguation** — a by-type lookup whose return type is generic
>    (`NumberStore<Double>` vs `NumberStore<Float>`, both erase to `NumberStore`) does
>    `getBean(NumberStore.class)` → `NoUniqueBeanDefinitionException` (2 candidates). Real Spring
>    uses the *generic* return type via `getBeanProvider(ResolvableType.forMethodReturnType(m))`.
>    Affects `withGenericBean` + the `*WithoutMetadataCaching` cases (1 in LookupMethodTests, 3 in
>    LookupAnnotationTests).
> 2. **`@Lookup` null-bean** — `withNullBean` expects a `null` result; our override throws/CCEs
>    instead of honouring Spring's `NullBean` marker. Also needs overload-aware override mapping:
>    `get()` is `@Lookup("testBean")` (by name) but collides with the overloaded `@Lookup get(String)`
>    (by type) in a name-keyed map (1 test).
>
> **Attempted 2026-06-21 (NOT merged):** a generic-aware emitter (by-type →
> `getBeanProvider(ResolvableType.forMethodReturnType(getClass().getSuperclass().getDeclaredMethod(...)))`,
> overload-aware mapping keyed on the `@Lookup` `Method`'s param count, and a `NullBean`→`null` unwrap)
> **does fix the generic case** — `withGenericBean` passes in both classes. It was **reverted** because
> the extra per-bean reflection (a) reliably triggers a **separate flaky JIT bug** — `InternalError:
> JIT dispatch into org/junit/jupiter/engine/execution/InterceptingExecutableInvoker.invokeVoid … not
> implemented: expected int on stack, got ref` (a JIT calling-convention type-tracking defect in the
> JUnit invoker, masking ~3 tests/class and net-regressing `LookupMethodTests` 6→4 in JIT mode), and
> (b) **hangs the interpreter under `--nojit`** (no RESULT, rc=1). Both are pre-conditions to resolve
> before the generic emitter can land. The JIT `invokeVoid` "expected int got ref" defect is a
> standalone find worth its own fix (it is a test-harness path, not production Spring).
>
> (`FactoryBeanTests` stays 4/6 — its 2 failures are an unrelated `${myName}` placeholder
> resolution issue, not method injection.)

> **FIX (landed):** both loaded-set-only bean filters in
> `native-builtins/src/spring_startup_bootstrap.rs` — the `getBeanClassName()` override
> (`abstract_bean_definition_get_bean_class_name`) and the `[bean-orphan]` skip — now add a
> `ctx.find_resource("{internal}.class")` classpath probe (no clinit, short-circuited after
> the loaded-set checks). A class that is on the classpath but not yet loaded (lazy nested /
> method-injection / proxied bean classes) is no longer hidden, so `getBeanClassName()` returns
> the real name and the bean instantiates. Spring infra beans (`Proxy{Caching,Transaction,Async}
> Configuration`, `internalAutoProxyCreator`, …) flip from FAIL to OK; ~100+ failures expected
> to clear.
>
> **Verified on the fixed binary:** the `[bean-filter] hiding bean class …` warning is **gone**
> for `LookupMethodTests` — the class is no longer hidden, so `getBeanClassName()` returns the
> real name. No regression on previously-OK bean tests.
>
> **Residual — bug-B2 (separate, OPEN):** `LookupMethodTests` still fails with "Target object
> must not be null" because the bean instance itself comes back **null** — CratonVM's CGLIB
> **method-injection / `@Configuration` subclass instantiation** (HotSpot makes
> `…AbstractBean$$SpringCGLIB$$0`) is a **distinct, deeper** defect not addressed by the filter
> fix. Re-run the full suite on the fixed binary to measure how many of the ~100+
> "Target object must not be null" failures were pure-filter (now fixed) vs CGLIB (bug-B2).

## Symptom
~24 bean/AOP test classes fail with `Target object must not be null`. CratonVM stderr
shows a CratonVM-specific intervention firing during context refresh:
```
[bean-filter] hiding bean class 'org.springframework.beans.factory.support.LookupMethodTests$AbstractBean' — not loadable on partial classpath
```
HotSpot, **same classpath**, instantiates the bean fine (CGLIB method-injection subclass
`LookupMethodTests$AbstractBean$$SpringCGLIB$$0`).

## CratonVM stack (standalone repro)
```
== [0] BeanCreationException: … 'abstractBean' …: Target object must not be null
   at AbstractAutowireCapableBeanFactory.instantiateBean(…:1342)
== [1] IllegalArgumentException: Target object must not be null
   at org.springframework.util.Assert.notNull(Assert.java:182)
   at AbstractNestablePropertyAccessor.setWrappedInstance(…:175)
   at BeanWrapperImpl.<init>(BeanWrapperImpl.java:94)
   at AbstractAutowireCapableBeanFactory.instantiateBean(…:1337)   ← wraps a NULL instance
```
The instance handed to `new BeanWrapperImpl(instance)` is **null**.

## Root cause (CratonVM native, confirmed in source)
`native-builtins/src/spring_startup_bootstrap.rs` registers a native override of
`AbstractBeanDefinition.getBeanClassName()`
(`abstract_bean_definition_get_bean_class_name`, registered at line ~1820). It reads the
bean's class name, then decides "loadable" using **only the already-loaded class set**:

```rust
// spring_startup_bootstrap.rs:2008
let internal = name.replace('.', "/");
let loadable = ctx.class_id_by_name(&internal).is_some()
            || ctx.class_id_by_name(&name).is_some();
if !loadable {
    warn!("[bean-filter] hiding bean class '{}' — not loadable on partial classpath", name);
    return Ok(Some(Value::Object(None)));   // ← getBeanClassName() returns NULL
}
```

`class_id_by_name` is a **loaded-set lookup**, not a classpath-resolvability check. The code
comment deliberately avoids `load_class()` and *assumes* "by the time Spring reaches an
instantiation site … `resolveBeanClass` will already have a ClassId". That assumption is
**false** for lazily-loaded nested / method-injection / proxied bean classes:
`LookupMethodTests$AbstractBean` is not yet loaded when `getBeanClassName()` is consulted
on the instantiation path, so it is wrongly judged not-loadable → `getBeanClassName()`
returns null → Spring instantiates a null instance → `BeanWrapperImpl(null)` → assert.

This is the same **loaded-set ≠ loadable** pattern as bug-06 fam3
(`findLoadedClass must not load`).

## Reproduce
```bash
VM=/c/craton/spring-vmbin/cratonvm-spring0618.exe
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
CP="<spring-suite>;<spring-beans/build/cratonvm-testcp.txt>"
# standalone repro (spring-suite/LookupRepro.java):
"$VM" --java-home "$JDK" -cp "$CP" LookupRepro     # CV: Target object must not be null
"$JDK\bin\java.exe"      -cp "$CP" LookupRepro     # HS: GOT abstractBean = …$$SpringCGLIB$$0
# or the test class:
"$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.beans.factory.support.LookupMethodTests
```

## Fix direction
Make the "loadable" probe reflect actual **classpath presence**, not the loaded set:
- check for the class *resource* (`name.replace('.','/') + ".class"`) via the app
  classloader / classpath index without running clinit, **or**
- make the filter conservative — when `class_id_by_name` misses, **do not hide**; return the
  real name and let Spring's normal resolution (and genuine `ClassNotFoundException` for
  truly-absent classes) run, matching HotSpot.

The whole `[bean-filter]` "partial classpath" feature is the suspect; it should only hide a
bean when the class is *provably absent*, never merely "not yet loaded".

## Blast radius / expected impact
~24 classes in this run carry `Target object must not be null` (spring-beans + spring-aop +
some context XML-config tests). A correct loadability probe should flip most to OK. Because
this fires on any not-yet-resolved bean class, it can also affect **real apps** (full
classpath), not just the partial-classpath test harness.

## Notes
- Related: [[bug06-fam4-synthetic-object-super-fam5-trap]] (loaded-set vs loadable),
  `SingletonTargetSource`/`BeanWrapperImpl` are only the messengers.
- Distinct from bug-A (off-heap Unsafe) — different subsystem.
- Some co-hidden names (`beans.testfixture.beans.TestBean`) may be genuinely off the test
  classpath; those are a separate test-fixture-packaging concern, not this bug.
