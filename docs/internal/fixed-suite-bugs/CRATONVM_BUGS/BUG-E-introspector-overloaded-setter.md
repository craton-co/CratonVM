# BUG-E — `java.beans.Introspector` picks the wrong overloaded setter / property type

**Test:** `jakarta.el.TestBeanSupport` (variant `[0: useStandalone[false]]`)
**Symptom under CratonVM:** `FAILURES!!!  Tests run: 26,  Failures: 5` — HotSpot: PASS.

```
testOverLoadedWithGetABean   expected:<TypeAAA> but was:<TypeA>     (write method param type)
testOverLoadedWithGetAABean  expected:<TypeAAA> but was:<TypeA>
testOverLoadedWithGetAAABean expected:<TypeAAA> but was:<TypeA>
testAmbiguousBean02          expected:<TypeA>   but was:<String>    (property/write type)
testMismatchBean             expected null, but was:<setValue(String)> (write method should be null)
```

## Root cause

The `useStandalone[false]` parametrization of `TestBeanSupport` routes through
`jakarta.el.BeanSupportFull`, which calls **`java.beans.Introspector.getBeanInfo()`**.
CratonVM does not run the real `java.beans.Introspector` bytecode — it provides a
native re-implementation (`introspector_get_bean_info` in
`native-builtins/src/phases_late.rs`). That native only kept the **first** getter
and the **first** setter it saw for each property and never reconciled overloaded
setters against the getter type, so:

- with overloaded `setValue(TypeA/TypeAA/TypeAAA)` it kept whichever setter
  appeared first in `declared_methods` order instead of the JDK's choice;
- a setter whose parameter type is incompatible with the getter return type
  (`MismatchBean`: `TypeA getValue()` + `setValue(String)`) was still attached
  as the write method instead of being rejected;
- with two unrelated setters and no getter (`AmbiguousBean02`) the result
  depended on class-file method order rather than the JDK's deterministic rule.

## The JDK / `BeanSupportStandalone` rule (now implemented)

Ground-truthed against JDK 25 (`Introspector.getBeanInfo`) and Tomcat's own
`jakarta.el.BeanSupportStandalone.getWriteMethod()/getType()`:

1. **Read method** = `getXxx()` / `isXxx()`; a boolean `isXxx()` takes precedence
   and locks out a plain `getXxx()`.
2. **Write method**: collect *all* `setXxx(T)` setters. Seed a candidate `type`
   with the getter's return type, or — when there is no getter — the
   lexicographically-smallest parameter type *binary name*. Then walk every
   setter: if `type.isAssignableFrom(param)`, adopt that more-derived `param` and
   make it the write method. A setter incompatible with the getter type is never
   selected (so `MismatchBean` correctly yields a `null` write method).
3. **Property type** = read method return type, else the chosen setter's
   parameter type.

## Fix

`introspector_get_bean_info` now accumulates per-property a read method (with
is/get precedence), the full list of setters (mirror + parameter
class id), and resolves the write method + property type with the
assignable-chain walk above. Static methods are excluded (JavaBeans are instance
properties).

Verified: `TestBeanSupport` 26/26 PASS, matching HotSpot. Property-resolution
ground truth captured with `.tooling/BeanProbe.java` / `BeanProbe2.java`.
