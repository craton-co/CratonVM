---
name: spring-boot-groovy-indy-mockito-mock-dispatch
description: PARTIAL. SpringRepositoriesExtensionTests tail. After 3c/3d (indy guard AIOOBE + void poly-invoke) and 3e (Groovy-truth cast:(Object)Z) and 3f (filterArguments closure→SAM coercion) were all FIXED, the test sits at 4/11. Two layers remain OPEN: (g) the Groovy-generated SAM proxy's execute() throws NoSuchMethodError (proxy method→InvocationHandler dispatch), and (h) closure→Action coercion on a Mockito mock drives no interaction.
metadata:
  type: known-issue
  area: invoke, groovy, indy, proxy, mockito
---

# SpringRepos tail — Groovy closure→SAM proxy dispatch + Mockito-mock SAM coercion

**Status:** 🟡 PARTIAL — `4/11` and climbing.
`org.springframework.boot.build.groovyscripts.SpringRepositoriesExtensionTests`
(Spring Boot buildSrc). The earlier layers are all FIXED; see
[[spring-boot-groovy-indy-runtime-argcount-3c-FIXED]],
[[reference_springrepos_indy_3c_3d_guardwithtest]], and
[[springrepos-extension-hang-jit-throughput-and-deep-recursion]].

## Layer ledger (this test)
| Layer | Bug | Status |
|---|---|---|
| 1 | ANTLR JIT miscompile → parse NPE | ✅ FIXED (JIT ban) |
| 2 | TypeVariable resolution → Mockito generics | ✅ FIXED |
| 3/3b | MethodHandle.type() receiver + insertArguments/asCollector arity | ✅ FIXED |
| 3c | `guardWithTest` dropped receiver → `sameClasses` AIOOBE | ✅ FIXED+MERGED (`5d36c432`) |
| 3d | Object-returning poly-invoke of a `void` target → operand-stack underflow | ✅ FIXED+MERGED (`5d36c432`) |
| 3e | Groovy truth: `cast:(Object)Z` not routed through `asBoolean` → falsy non-null read as true | ✅ FIXED (`cd4bfb27`, route to `DefaultTypeTransformation.castToBoolean`) |
| 3f | `MethodHandles.filterArguments` was a passthrough → closure→SAM coercion never ran | ✅ FIXED (`1b1f8cde`, real `MH_KIND_FILTER`) |
| **g** | **Generated SAM proxy's `execute()` → `NoSuchMethodError`** | 🔴 OPEN |
| **h** | **closure→Action coercion on a Mockito mock drives no interaction** | 🔴 OPEN |

Net: `0/11` (all crashed) → **`4/11`** clean. The 4 passing are the empty/false-
condition cases; the 7 failing all need `maven.mavenContent { }` / `maven.content
{ }` / `maven.credentials { }` (closures coerced to a Gradle `Action`) to drive
their Mockito stubs, which they don't yet.

## Layer g — SAM proxy `execute()` → NoSuchMethodError
With 3f, Groovy's `TypeTransformers.createSAMTransform` now runs and wraps the
`Closure` in a JDK dynamic proxy implementing `Action` (`jdk/proxy1/$Proxy0`).
But invoking the SAM method on it fails:

```
NoSuchMethodError: jdk/proxy1/$Proxy0.execute(Ljava/lang/Object;)V
  caller: SamRealProbe$RealSink.mavenContent(Lorg/gradle/api/Action;)V
```

So CratonVM resolves `execute` as a concrete method on the generated proxy class
and finds none, instead of routing the call to the proxy's `InvocationHandler`
(Groovy's `ConvertedClosure`, which would call `closure.call(args)`). This is a
`java.lang.reflect.Proxy` method-dispatch gap for proxies whose interface method
is invoked from compiled Java/Groovy (cf. [[reference_proxy_realsuper_soak]]).
Repro: `docs/internal/repros/springrepos-indy-3c/SamRealProbe.java` (real, non-
mock `Action` sink).

## Layer h — closure→Action on a Mockito mock
`docs/internal/repros/springrepos-indy-3c/SamCoerceProbe.java`: `repo.mavenContent
{ }` where `repo = mock(MavenArtifactRepository.class)` and
`given(repo.mavenContent(any(Action.class)))` is stubbed. On CratonVM the stub is
never driven (`fired=0`), so the test's `mavenContent`/`content`/`credentials`
descriptors stay empty → `verify(mavenContent.get(0))…` throws
`ArrayIndexOutOfBoundsException`. Either the closure→Action coercion isn't applied
on the mock-targeted call (so no/ wrong-typed arg), or the coerced `Action` call
doesn't reach Mockito's ByteBuddy interceptor. Note the plain `maven(Closure)`
overload on the same mock DOES fire (it has a `Closure` overload, no coercion
needed), so basic Mockito interception works — the gap is specific to the
SAM-coerced path.

## How to reproduce
```bash
CV=<cvindy3c.exe>; JH="C:/Program Files/Java/jdk-25"
cd apps/spring-boot/buildSrc; CP="runner;$(cat test-classpath.txt)"
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" --java-home "$JH" --nojit -cp "$CP" \
  RunJUnit org.springframework.boot.build.groovyscripts.SpringRepositoriesExtensionTests
# tests=11 passed=4 failed=7; failures = ArrayIndexOutOfBoundsException at verify(...get(0))
# Isolated: "$CV" ... -cp "$CP" SamRealProbe   (layer g: proxy.execute NoSuchMethodError)
#           "$CV" ... -cp "$CP" SamCoerceProbe (layer h: mock fired=0)
```

## Where to look
- Proxy dispatch: `java/lang/reflect/Proxy` handling in the interpreter / native
  proxy generation (does the generated `$ProxyN` route interface methods to its
  `InvocationHandler`? `execute` must reach `ConvertedClosure.invoke`).
- SAM transform: Groovy `org/codehaus/groovy/vmplugin/v8/TypeTransformers`
  (`createSAMTransform`, `TO_REFLECTIVE_PROXY`/`TO_GENERATED_PROXY`,
  `ProxyGenerator`).
- Combinators: `native-builtins/src/lang_invoke.rs` (`MH_KIND_FILTER` /
  `mh_dispatch_filter` landed in 3f).

## Tools / artifacts
Probes (all under `docs/internal/repros/springrepos-indy-3c/`): `SamRealProbe`
(layer g), `SamCoerceProbe` (layer h); plus the 3e probes `CastProbe`/`NegProbe`/
`BoolReturnProbe`/`DttProbe` (== HotSpot). buildSrc tree is gitignored; runner
copies live in `apps/spring-boot/buildSrc/runner/`.
