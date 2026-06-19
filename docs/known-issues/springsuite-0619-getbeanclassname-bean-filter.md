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
| **CratonVM HEAD** | found `c4536b94`; **FIXED on dev `3b16e985`+** (this session) |
| **Status** | **FIXED** on dev — bean-filter now probes the classpath, not the loaded-set |
| **Suggested owner** | done (landed on dev) |

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
