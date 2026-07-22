# Bug 07 — BeanELResolver: bean property not discovered  (FIXED)

**Status:** FIXED (commit on fix/tomcat-suite-loop). `TestBeanELResolver` now
OK(156) (was Failures:1); `TestBeanSupport` OK(26) unchanged (no regression).

## Fix

`introspector_get_bean_info` (phases_late.rs) walked only the `superclass_of`
chain, so getters that are interface **default methods** were never seen. It now
also traverses all transitively-implemented interfaces (BFS via
`class_interfaces`), appended after the class/superclass chain so concrete
overrides still win the dedup — matching `java.beans.Introspector` /
`Class.getMethods()`. The `valueC` property (a `default String getValueC()` on
`MyInterface`) is now discovered.

---
_Original report:_

**Was:** OPEN. Real CratonVM semantic bug (HotSpot PASSes).
**Severity:** Medium — EL bean-property introspection gap; affects EL/JSP.
**Repro class:** `jakarta.el.TestBeanELResolver` — `Tests run: 156, Failures: 1`
(`testGetDefaultValue[0: useStandalone[false]]`).

## Symptom

```
jakarta.el.PropertyNotFoundException: Property [valueC] not found on type
  [jakarta.el.TestBeanELResolver$Bean]
  at jakarta.el.BeanELResolver$BeanProperties.get(BeanELResolver.java:188)
  at jakarta.el.BeanELResolver.property(BeanELResolver.java:259)
  at jakarta.el.BeanELResolver.getValue(BeanELResolver.java:84)
  at jakarta.el.TestBeanELResolver.testGetDefaultValue(TestBeanELResolver.java:918)
```

## Root cause (hypothesis)

`BeanELResolver$BeanProperties` introspects the bean's properties (via the JDK
`Introspector` / CratonVM's `introspector_get_bean_info` native, see
phases_late.rs). Property `valueC` is present on the bean (a getter/setter the
test expects) but CratonVM's bean-info introspection does not surface it — likely
a getter/setter whose shape CratonVM's property-merge logic misses (e.g. a
covariant/boxed return, a default-method accessor, or a getter-only/`is`-prefixed
boolean property). This is the same family as the earlier Tomcat bug-E
(Introspector overloaded-setter/property-type merge); `valueC` is a case that
fix did not cover.

`useStandalone[false]` = the JDK-Introspector path (vs `BeanSupportStandalone`),
so the gap is in CratonVM's `java.beans.Introspector` emulation specifically.

## Next steps

- Inspect `TestBeanELResolver$Bean` (test source ~line 918) to see how `valueC`
  is declared, then compare CratonVM `introspector_get_bean_info` output vs
  HotSpot for that bean.
- Likely fix in `phases_late.rs::introspector_get_bean_info` property collection.

## Reproduction

```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  jakarta.el.TestBeanELResolver        # CWD: apps/tomcat
# -> Tests run: 156, Failures: 1 ; HotSpot: PASS
```
