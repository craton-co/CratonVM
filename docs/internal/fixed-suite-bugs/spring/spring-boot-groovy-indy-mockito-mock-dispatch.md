---
name: spring-boot-groovy-indy-mockito-mock-dispatch
description: PARTIAL. SpringRepositoriesExtensionTests tail. After 3c/3d (indy guard AIOOBE + void poly-invoke) and 3e (Groovy-truth cast:(Object)Z) and 3f (filterArguments closure→SAM coercion) were all FIXED, the test sits at 4/11. Two layers remain OPEN: (g) the Groovy-generated SAM proxy's execute() throws NoSuchMethodError (proxy method→InvocationHandler dispatch), and (h) closure→Action coercion on a Mockito mock drives no interaction.
metadata:
  type: known-issue
  area: invoke, groovy, indy, proxy, mockito
---

# SpringRepos tail — Groovy closure→SAM proxy dispatch + Mockito-mock SAM coercion

**Status:** ✅ FIXED — **`11/11` FULL GREEN.** All layers resolved: Groovy/indy
(1–3f, g1), Module access (g2), Mockito inline-mock create+dispatch (h1 addReads0,
h2 `ConcurrentHashMap(Map)` ctor), and the `%S` format conversion (h3). Inline
mocking now functions end-to-end on CratonVM. NOTE: the first inline-mock
retransformation (ByteBuddy `Advice` weaving) is slow — under the default 120s
`--stack-dump-on-timeout` watchdog the run aborts mid-weave; pass
`--stack-dump-on-timeout=0` (or a larger value) to let it complete. That's the
known interpreter throughput wall, not a hang.
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
| g1 | `MethodHandles.foldArguments` was a passthrough → `TypeTransformers.TO_REFLECTIVE_PROXY` malformed → SAM proxy built with **no interfaces** (`execute()` → `NoSuchMethodError`) | ✅ FIXED (real `MH_KIND_FOLD`; SAM proxy now has `[Action]`, `execute` dispatches to the closure — verified by `SamRealProbe`) |
| g2 | Named `Module` (e.g. `java.base`) has a null `descriptor` → real `implIsExportedOrOpen` NPEs in every reflective access check (Groovy `CachedClass.getMethods`, `GroovySystem.<clinit>`, JUnit `@BeforeAll`) | ✅ FIXED — `Module.isExported/isOpen` natives (still force-listed) now answer from the boot `ModuleRegistry`'s **accurate** per-module exports/opens instead of blanket-`true`; the null descriptor is never touched. `java.base.isExported("java.lang")==true` (Groovy works) and `isExported("jdk.internal.misc")==false` (ByteBuddy's `JavaDispatcher` gets the real-JDK answer, so it does NOT regress). Byte-identical to HotSpot (`ModProbe`). Also: `is_package_{exported,open}_unqualified` now honor automatic modules. **Replaces the reverted blanket-permissive attempt.** |
| g3 | ByteBuddy `JavaDispatcher.<clinit>` → `IllegalStateException: Failed to create invoker` (reached once g2's NPE is bypassed) | 🟡 should be unblocked by the accurate g2 fix (JavaDispatcher now sees `jdk.internal.*` as not-exported and takes its fallback); not separately re-verified — SpringRepos now fails further along at the Mockito **inline-mock** wall (layer h) |
| h1 | Mockito inline-mock **creation** aborted: `assureCanReadMockito` → `Instrumentation.redefineModule` → `Module.implAddReads` → **unregistered `addReads0`** `UnsatisfiedLinkError` → "Could not modify all classes" | ✅ FIXED — registered `java/lang/Module.addReads0(Module,Module)V` (native-builtins/lib.rs) to mirror the read edge into the boot `ModuleRegistry`. Inline mocks now CREATE; SpringRepos `0/11`→**`3/11`** (stable). |
| h2 | `mockingDetails(m).isMock()==false` and `when(m.foo())`/`given(m)` → `NotAMockException` even though the mock dispatches (`m.foo()` records an ongoing stubbing). ROOT (NOT a ByteBuddy/advice problem): `MockUtil`'s static `mockMakers = new ConcurrentHashMap<>(Collections.singletonMap(makerClass, defaultMaker))` came up **EMPTY** because CratonVM's synthetic `ConcurrentHashMap.<init>(Map)` (`native_chm_init_from_map`) read entries via the HashMap-bucket-only `map_collect_entries`, which returns nothing for a `Collections$SingletonMap`/`TreeMap` source. So `getMockHandlerOrNull` iterated an empty `mockMakers.values()` and never found the handler. | ✅ FIXED — `native_chm_init_from_map` now uses `collect_entries_any` (the layout-agnostic `entrySet()` walk, same as `native_chm_put_all`). General fix: `new ConcurrentHashMap<>(singletonMap/treeMap/anyMap)` now copies entries. SpringRepos `3/11`→**`9/11`**. |
| h3 | The injected `environment` `UnaryOperator<String>` looked up the wrong env-var key, so the Groovy script fell back to the real default repo URL (`setUrl("https://…broadcom…")` vs wanted `setUrl("url")`). ROOT (NOT a closure-dispatch bug): `fromEnv` builds the key with `"COMMERCIAL_%SREPO_URL".formatted(id)`, but CratonVM's `String.format` left the uppercase `%S` conversion **literal** (only `%X`/`%E`/`%G`/`%A`/`%H` were handled), so the key was `COMMERCIAL_%SREPO_URL` and never matched. | ✅ FIXED — recognize `%S`/`%B`/`%C` in the format specifier parser and upper-case the result in `format_arg_full` (lang_string.rs). SpringRepos `9/11`→**`11/11`** (FULL GREEN). |

Net: `0/11` (all crashed) → **`4/11`** clean *(flaky: the g2 Module NPE in
`GroovySystem.<clinit>` intermittently drops it to `0/11`)*. The 4 passing are the
empty/false-condition cases; the 7 failing all need `maven.mavenContent { }` /
`maven.content { }` / `maven.credentials { }` (closures coerced to a Gradle
`Action`) to drive their Mockito stubs.

**Update 2026-06-24:** with the g2 descriptor NPE fixed AND the boot module
registry populated (`2ec37261`), the test now reaches `createExtension`
(`Mockito.mock(RepositoryHandler)`) on *every* case and fails there at the
**Mockito inline-mock** wall — not the Groovy NPE. Mockito's
`InlineBytecodeGenerator.assureCanReadMockito` finds `java.base` does not read
the unnamed module, tries `InstrumentationImpl.redefineModule` →
`UnsatisfiedLinkError: java/lang/Module.addReads0`, then `TypeCache.findOrInsert`
→ `MockitoException: Could not modify all classes […RepositoryHandler…]`. This is
class **retransformation/redefinition** support (instrumentation), independent of
the SAM/indy work — the real remaining blocker for this test. (A force-listed
permissive `Module.canRead` would skip the `addReads0` path but inline mock
generation still needs class redefine, so it would not by itself make the test
pass.) The descriptor-NPE regression that this fix targets is resolved:
`ArtifactReleaseTests` CRASH→**`8/8`**, `DocumentAutoConfigurationClassesTests`
`0/2`→**`2/2`** (both fully green after the two fixes below).

**Update 2026-06-24 (Mockito inline-mock dig — `0/11`→`3/11`):** the `addReads0`
`UnsatisfiedLinkError` (layer h1) was the *first* domino — it aborted
`assureCanReadMockito` *before* `retransformClasses` ever ran. Registering the
native (it mirrors the read edge into the `ModuleRegistry`; Java-side
`implAddReads` already updates the heap `reads` set) lets the whole inline-mock
**creation** path complete: SpringRepos `0/11`→**`3/11`** (stable, no flakiness —
that was the now-fixed g2 NPE). The remaining 8 fail at layer h2: mocks are
*generated* but *inert*. Isolated probes (`mock(I.class)` then `when(m.foo())`)
reproduce `NotAMockException` / `isMock()==false` on a `…$MockitoMock$…` instance.
With `CRATONVM_DBG_RETRANSFORM=1` the pipeline shows `retransformClasses0` running
on the interface (original bytes present) and the registered ByteBuddy transformer
returning bytes — but the actual interface-mock dispatch lives in the generated
`$MockitoMock$` *subclass* (defined separately, never entering the retransform
set), whose `MockMethodAdvice`-woven method bodies don't reach
`MockMethodDispatcher`. That subclass-advice→dispatcher routing is the open work;
the instrumentation/retransform infra itself is sound.

**Update 2026-06-24 (residual MethodHandle unboxing bug — FIXED):** the
`ArtifactReleaseTests`/`DocumentAutoConfigurationClassesTests` residual was NOT a
`ProjectBuilder` NPE but a `MethodHandle` defect. Gradle's
`DefaultLegacyTypesSupport.injectEmptyInterfacesIntoClassLoader` ASM-generates
each empty interface (~91 bytes) and defines it through
`ClassLoaderUtils$LookupClassDefiner`, which does
`findVirtual(ClassLoader,"defineClass",(String,byte[],int,int)Class).bindTo(cl)
.invokeWithArguments(new Object[]{name, bytes, 0, bytes.length})`. CratonVM's
`mh_dispatch` virtual/bound arm (`lang_invoke.rs`) called `invoke_virtual`
WITHOUT unboxing the boxed `Integer` args (the `MH_KIND_STATIC` arm already did,
via `adapt_invoke_args`), so both `int` params read **0** → `defineClass1` saw a
0-length array → "class file too short (0 bytes)" → "Could not inject synthetic
classes". Fix: apply `adapt_invoke_args` in the virtual/bound arm too (idempotent
for the direct invoke/invokeExact path). Isolated probe
(`bindTo(...).invokeWithArguments` with boxed ints) now byte-matches HotSpot
(`8001`), 23 invoke unit tests pass. This is broad — any bound virtual `MH`
invoked with primitive args via `invokeWithArguments`/`Object[]` was affected.

## Layer g1 — `foldArguments` passthrough (FIXED)
Groovy's `TypeTransformers.TO_REFLECTIVE_PROXY` =
`foldArguments(Proxy.newProxyInstance…, new ConvertedClosure(closure,name))`.
CratonVM's `MethodHandles.foldArguments` returned the target unchanged (ignored
the combiner), so the `ConvertedClosure` handler was never built and the arg
vector shifted → `Proxy.newProxyInstance` got an **empty interfaces array** →
the proxy implemented nothing → `execute()` 404'd. Implemented a real
`MH_KIND_FOLD` adapter (run combiner over its param count starting at `pos`,
splice a non-void result in at `pos`, dispatch target). `SamRealProbe` now shows
`interfaces=[Action]`, `execute OK`, closure ran. (`filterReturnValue` /
`collectArguments` remain passthroughs — likely the next combinator gaps.)

## Layer g2 — named Module has null descriptor (OPEN; permissive fix regresses ByteBuddy)
Once g1 is fixed, `createSAMTransform` reflects via `CachedClass.getMethods` →
`ReflectionUtils.checkCanSetAccessible` → `Java9.checkAccessible` →
`Module.isExported(pn, other)` → real `implIsExportedOrOpen` →
`descriptor.isOpen()` **NPE** because CratonVM's `Module` mirrors carry a name
but no `ModuleDescriptor`. This is pre-existing and **flaky** (it also crashes
`GroovySystem.<clinit>` and JUnit `@BeforeAll`, which is why the test oscillates
4/11 ↔ 0/11). A blanket-permissive native (`isExported`/`isOpen` → true via the
empty module registry, force-listed) **removes the NPE** and makes layer g1's
SAM path fully work — BUT it then **regresses ByteBuddy**: `JavaDispatcher` queries
`java.base.isExported("jdk.internal.…")`, expects the real-JDK answer (`false`,
so it uses a fallback strategy), and our always-`true` makes it attempt direct
access that fails (layer g3). The correct fix needs **accurate per-module export
modeling** (java.base exports `java.lang`/`java.util`/… but NOT `jdk.internal.*`/
`sun.*`), not blanket-true. NOTE the existing `isExported`/`isOpen` natives
(`phases_late::register_p59_module`) are registered ONLY via
`register_synthetic_overrides` — a no-op in the default real-JDK build — so they
must ALSO be added to `register_essential_natives` once a correct policy exists.
The blanket-permissive attempt was reverted; only g1 (`foldArguments`) landed.

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
