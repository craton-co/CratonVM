# Spring AOP: lambda beans not auto-proxied (hidden-class pointcut matching)

**Status:** ✅ RESOLVED

## Symptom

`org.springframework.aop.aspectj.autoproxy.AspectJAutoProxyCreatorTests` — 3 of 22
test methods failed on CratonVM real-JDK jit-on mode vs HotSpot:

- `nullAdviceIsSkipped()`
- `lambdaIsAlwaysProxiedWithJdkProxy(Class)` — both parameterizations
  (`ProxyTargetClassFalseConfig`, `ProxyTargetClassTrueConfig`)

All three failed at `AopUtils.isAopProxy(supplier)` returning `false`: Spring's
`AnnotationAwareAspectJAutoProxyCreator` decided NOT to proxy a `Supplier<String>`
lambda bean at all, because it found no eligible advisor for it. (Two sibling
tests in the same class — `twoAdviceAspectPrototype`/`twoAdviceAspectSingleton` —
were a *different*, already-fixed bug: `find_loaded_class_for_loader` hiding a
generated JDK proxy from its own built-in defining loader. See dev commit
`99cc206b`.)

## Root cause

`Class.getGenericInterfaces()` returned an **empty array** for lambda-proxy
classes on CratonVM (`native_class_get_generic_interfaces`,
`../../../../native-builtins/src/lang_class.rs`), while HotSpot correctly returns
`[interface java.util.function.Supplier]` for the same lambda's `Class` object.

`native_class_get_interfaces()` (backing `Class.getInterfaces()`) already had a
special case for lambda-proxy classes — CratonVM doesn't register a lambda's
synthetic class in the class manager, so it looks up the lambda's SAM
(functional) interface via `ctx.lambda_functional_interface(class_id)` and
returns `[SAM]`. `native_class_get_generic_interfaces()` was missing this exact
same special case: with no generic `Signature` attribute (lambdas don't have
one) it fell through to the generic fallback `ctx.class_interfaces(class_id)`,
which — like everything else keyed off the class manager — returns empty for a
class id that was never registered there.

A related bug in the same vein: `Class.getPackageName()`
(`native_class_get_package_name`) also returned an **empty string** for
lambda-proxy classes (vs HotSpot's correct host-class package), for the exact
same reason — it resolves the package via `class_name_of_id`/`mirror_class_name`,
both of which miss the class-manager lookup for a lambda-proxy id.

Spring's `AspectJExpressionPointcut`/`AopUtils.canApply` — via aspectjweaver's
reflection-based pointcut matching for `execution(* java.util.function.Supplier+.get())`
— consults `getGenericInterfaces()` (not just `getInterfaces()`) when resolving
a method's declaring-class supertype closure for subtype ("+") matching. With
an empty interfaces array, AspectJ concluded the lambda implemented no
interfaces at all and returned `neverMatches()` for every candidate method —
`AopUtils.canApply` found no eligible advisor, so
`AnnotationAwareAspectJAutoProxyCreator` never wrapped the lambda bean in a
proxy.

## Fix

Added the same `ctx.lambda_functional_interface(class_id)` special case to
`native_class_get_generic_interfaces()` that `native_class_get_interfaces()`
already had (return `[SAM]` as a plain, non-generic `Class[]`/`Type[]`, matching
the JDK contract that `getGenericInterfaces()` returns the raw interface list
when there's no generic signature to parse).

Added a lambda-proxy special case to `native_class_get_package_name()`: derive
the package from the lambda's host class name (via `ctx.lambda_proxy_host`)
instead of falling through to the class-manager-backed lookup that has no entry
for a lambda-proxy id.

## Investigation trail (for context — dead ends ruled out before finding the real cause)

Before finding the actual bug, several plausible-looking hypotheses were tested
and ruled out via a ground-truth harness (a locally patched, recompiled copy of
`AspectJExpressionPointcut` with debug tracing added to `getShadowMatch`,
shadowing the real class earlier on the classpath) — confirming that
`PointcutExpression.matchesMethodExecution()` does **not** throw when called
through Spring's real code path, and directly returns `neverMatches()` on
CratonVM vs `alwaysMatches()` on HotSpot for the identical `Method` object.
Ruled out along the way:

- **Not** a `Class.getName()` / `getTypeName()` bug — both return the correct
  dotted lambda class name on CratonVM, matching HotSpot exactly.
- **Not** a Class-mirror identity/duplication bug — `Method.getDeclaringClass()
  == lambdaClass` is `true` on CratonVM.
- **Not** a `getMostSpecificMethod` resolution difference — identical on both
  JVMs.
- **Not** `Class.isHidden()` misreporting — both JVMs correctly report `true`,
  and `Class.forName` correctly throws `ClassNotFoundException` for the hidden
  class on both (per JVMS §5.3, hidden classes are never discoverable by name).
- A raw, standalone `aspectjweaver` `PointcutParser` reproduction (bypassing
  Spring's `AspectJExpressionPointcut` entirely) threw a `ReflectionWorldException`
  identically on both JVMs — this turned out to be an artifact of the simplified
  reproduction not matching Spring's exact `PointcutExpression` construction
  (missing `BeanPointcutDesignatorHandler` registration and/or other setup), and
  was a dead end / not representative of the actual code path Spring uses.

There is also a separate, minor **cosmetic** bug (not fixed here, low priority):
`Method.toString()` (`native_method_to_string`, `../../../../native-builtins/src/lang_reflect.rs`)
prints an empty declaring-class prefix for a lambda-proxy method (e.g.
`public java.lang.Object .get()` instead of `public java.lang.Object
Foo$$Lambda/0x....get()`), because it reads the name via `mirror_class_name()`,
which — unlike `native_class_get_name()` — doesn't special-case lambda-proxy
class ids via `lambda_proxy_class_name()`. Does not affect `getName()`/
`getTypeName()`, which already handle lambda proxies correctly, so it's display
-only.

## Repro

`apps/spring-suite-runner`; `CRATONVM_BIN=<built-vm> ./run-suite.sh run --jdk
real --jit on --batch 1 --only 'AspectJAutoProxyCreatorTests'` — 22/22 passing
after the fix (0 known regressions; spring-aop module regression sweep, plus
`SerializableTypeWrapperTests`/`MethodInvokingFactoryBeanTests` spot checks, all
clean).
