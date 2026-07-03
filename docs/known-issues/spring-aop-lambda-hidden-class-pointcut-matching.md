# Spring AOP: lambda beans not auto-proxied (hidden-class pointcut matching)

**Status:** OPEN (UNFIXED)

## Symptom

`org.springframework.aop.aspectj.autoproxy.AspectJAutoProxyCreatorTests` — 3 of 22
test methods fail on CratonVM real-JDK jit-on mode vs HotSpot:

- `nullAdviceIsSkipped()`
- `lambdaIsAlwaysProxiedWithJdkProxy(Class)` — both parameterizations
  (`ProxyTargetClassFalseConfig`, `ProxyTargetClassTrueConfig`)

All three fail at `AopUtils.isAopProxy(supplier)` returning `false`: Spring's
`AnnotationAwareAspectJAutoProxyCreator` decides NOT to proxy a `Supplier<String>`
lambda bean at all, because it finds no eligible advisor for it. (Two sibling
tests in the same class — `twoAdviceAspectPrototype`/`twoAdviceAspectSingleton` —
were a *different* bug, already fixed; see
`fix(classloader): findLoadedClass proxy visibility for own defining loader`.)

## Root-cause trail

1. `AopUtils.canApply(pointcut, lambdaClass)` (spring-aop) iterates the lambda's
   own class methods *and* its interfaces, testing each against the pointcut
   `execution(* java.util.function.Supplier+.get())`.
2. For the lambda's own concrete `get()` method (declaring class = the lambda's
   hidden class, e.g. `Foo$$Lambda/0x80000000`), `AspectJExpressionPointcut`
   (via aspectjweaver 1.9.25's `PointcutExpression.matchesMethodExecution`)
   throws `ReflectionWorld$ReflectionWorldException: can't determine superclass
   of missing type Foo$$Lambda.0x80000000` (note: aspectjweaver's own internal
   `/` → `.` "make dotted" conversion corrupts the literal `/0x...` suffix in
   the hidden-class name into `.0x...`, which cannot resolve back to the real
   class either way).
3. **This same exception reproduces identically on real HotSpot** when calling
   the raw `aspectjweaver` API directly (`PointcutParser` +
   `matchesMethodExecution`) — confirmed via an isolated repro using the exact
   same `PointcutParser` factory method, primitive set, and
   `pointcutDeclarationScope` that Spring's `AspectJExpressionPointcut` uses
   internally. So the underlying aspectjweaver limitation with hidden/lambda
   classes is **not CratonVM-specific**.
4. Yet Spring's `AopUtils.canApply` — going through the *real*
   `AspectJExpressionPointcut.matches(Method, Class, boolean)` wrapper (not the
   raw aspectjweaver API) — returns `true` on HotSpot and `false` on CratonVM
   for the **exact same inputs** (same lambda class, same declaring method, same
   `getMostSpecificMethod` resolution — verified identical on both JVMs).
   `AspectJExpressionPointcut.getShadowMatch()` catches
   `ReflectionWorldException` and has fallback logic
   (`getFallbackPointcutExpression` + a retry against the "original" method
   before `AopUtils.getMostSpecificMethod` resolution) — but in this scenario
   `targetMethod == originalMethod` already (the lambda's own `get()` IS already
   the most-specific method), so the "retry with original method" branch never
   fires on either JVM. Despite identical control flow up to this point, the
   final `shadowMatch` verdict differs between JVMs.

## What's ruled out (verified, not the cause)

- **Not** a `Class.getName()` / `Method.toString()` bug: `Class.getTypeName()`
  and `Class.getName()` both return the fully-correct dotted lambda class name
  on CratonVM, matching HotSpot exactly, including through
  `Method.getDeclaringClass().getTypeName()`. (There IS a separate, purely
  cosmetic bug where `Method.toString()`'s own native implementation
  (`native_method_to_string` in `native-builtins/src/lang_reflect.rs:815`) prints
  an *empty* declaring-class prefix for lambda-proxy classes — because it reads
  the name via `mirror_class_name()`, which does not special-case lambda-proxy
  `ClassId`s the way `native_class_get_name` does via `lambda_proxy_class_name()`
  — but this is display-only and does not affect `getTypeName()`/`getName()`,
  which lambda-proxy classes DO special-case correctly. Worth fixing separately
  as a cosmetic follow-up: route `mirror_class_name`/`mirror_class_name_strict`
  through the same `lambda_proxy_class_name()` check as `native_class_get_name`.)
- **Not** a Class-mirror identity/duplication bug: `Method.getDeclaringClass() ==
  lambdaClass` is `true` on CratonVM (single canonical mirror, not duplicated).
- **Not** a `getMostSpecificMethod` resolution difference: identical result
  (returns the lambda's own method, unchanged) on both JVMs.
- **Not** a `ShadowMatchUtils` cache-collision artifact from `canApply`'s
  iteration order (own-class methods enumerated before interface methods, so a
  degenerate self-match get cached under the same key AspectJ later reuses for
  the interface method): reproduced the exact 2-step call sequence on a single
  shared `AspectJExpressionPointcut` instance and the FIRST call
  (`matches(lambdaOwnGet, lambdaClass)`, the "trivial"/degenerate case with no
  fallback-retry involved at all) *already* disagrees between JVMs — HotSpot
  `true`, CratonVM `false` — before the cache-collision theory is even relevant.

## Where to look next

The divergence must be inside `ReflectionWorld`/`ShadowMatchImpl` construction
for the `MissingResolvedTypeWithKnownSignature` placeholder AspectJ falls back to
after the "missing type" exception — specifically why
`JoinPointSignatureIterator`/`SignaturePattern.matches` ultimately produces a
positive "maybe/always matches" verdict on HotSpot from a supposedly-failed type
resolution, but a firm "never matches" on CratonVM from the *same* failed
resolution. Plausible next steps:
- Instrument (temporarily) `ShadowMatchImpl`/`MissingResolvedTypeWithKnownSignature`
  inside aspectjweaver itself (decompile/patch a local copy, or attach a debugger)
  to see exactly which `FuzzyBoolean` value it derives post-exception on each JVM.
- Check whether CratonVM's `is_class_hidden()` (native-builtins) correctly
  reports lambda-proxy classes as hidden (`Class.isHidden()`), and whether
  aspectjweaver has version-gated special-casing for `Class.isHidden()` that
  takes a different path than the generic "missing type" placeholder — if so,
  the CratonVM divergence might be in some OTHER reflective query (annotations,
  generic signature, interfaces-of-declaring-class) that aspectjweaver consults
  when constructing the placeholder's fallback answer, not in name resolution
  at all.

## Repro

`apps/spring-suite-runner`; `CRATONVM_BIN=<built-vm> KRUN_STACK=1 ./run-suite.sh
run --jdk real --jit on --batch 1 --only 'AspectJAutoProxyCreatorTests'`.

Isolated Java repro (no suite needed) used during this investigation constructed
a fresh standalone `Supplier<String> lambda = () -> "x"` (bypassing Spring
entirely) and called `AopUtils.canApply(pointcut, lambda.getClass())` directly —
reproduces `false` on CratonVM / `true` on HotSpot with zero Spring machinery
involved beyond `spring-aop`'s own `AopUtils`/`AspectJExpressionPointcut`.
