---
name: spring-boot-groovy-indy-runtime-argcount-3c
description: HANDOFF — the last open layer (3c) blocking SpringRepositoriesExtensionTests. After fixing the MethodHandle receiver/type() and combinator type-tracking (layers 3/3b, all on dev), the Groovy invokedynamic dispatch still throws ArrayIndexOutOfBoundsException in IndyGuardsFiltersAndSignatures.sameClasses via IndyInterface.fromCache: the cached guard classes[] is LONGER than the runtime arguments[] Groovy collects, even though every MethodHandle.type() now matches HotSpot. A runtime arg-consistency / call-site-spread issue, not a type() issue. Repro + tools + candidate causes below.
metadata:
  type: known-issue
  area: invoke, groovy, indy
---

# SpringRepos layer 3c — Groovy indy runtime arg-count mismatch (`sameClasses` AIOOBE)

**Status:** 🔴 OPEN. This is the **last** known blocker for
`org.springframework.boot.build.groovyscripts.SpringRepositoriesExtensionTests`
(buildSrc). Parent cascade + the four fixed layers:
[[springrepos-extension-hang-jit-throughput-and-deep-recursion]].

## Where it fails
Under `--nojit` (fast, ~89s; no parse-NPE, no JIT) all 11 tests fail with:
```
[0] java.lang.reflect.UndeclaredThrowableException
[1] java.lang.reflect.InvocationTargetException
[2] java.lang.ArrayIndexOutOfBoundsException
  at org.codehaus.groovy.vmplugin.v8.IndyGuardsFiltersAndSignatures.sameClasses(IndyGuardsFiltersAndSignatures.java:226)
  at org.codehaus.groovy.vmplugin.v8.IndyInterface.fromCache(IndyInterface.java:321)
  at ...SpringRepositoriesExtensionTests$SpringRepositoriesExtension.lambda$get$0   (the JDK-proxy InvocationHandler)
  at ...$Proxy28.mavenRepositories(Unknown Source)
```
The call path: test → `$Proxy28.mavenRepositories()` (JDK dynamic proxy) →
InvocationHandler `lambda$get$0` reflectively `Method.invoke`s the Groovy
`SpringRepositoriesExtension.mavenRepositories()` → that body does
`addRepositories { }` via **invokedynamic** → `IndyInterface.fromCache` →
`sameClasses` → AIOOBE.

## Exact mechanism (decompiled Groovy 4.0.29)
`sameClasses(Class<?>[] cs, Object[] os)` loops `for i in 0..cs.length { … os[i] … }`
— so it AIOOBEs when **`cs.length > os.length`**.

In `fromCache`, `cs` = the **cached** guard's `classes[]` (bound at link time) and
`os` = the **current** call's `arguments` (`Object[]`, the last param of
`fromCache(MutableCallSite, Class, String, int, Boolean, Boolean, Boolean, Object,
Object[] arguments)`). So **the cached `classes[]` is longer than the `arguments[]`
CratonVM spreads for this call.**

The guard MH itself is built in `IndyInterface.make` as:
```
insertArguments(sameClasses, 0, classes)
   .asCollector(Object[].class, type.parameterCount())
   .asType(callSiteType)
```

## What is ALREADY fixed (so this is NOT a type() bug)
All on dev (branch `fix/springrepos-coldpath`, merged):
- **Layer 3** (`bcbe2f23`): `Lookup.unreflect`/`findVirtual` now include the leading
  **receiver** in a virtual/special `MethodHandle.type()`; `bindTo` drops it.
- **Layer 3b** (`894d718c`): `insertArguments`/`asCollector` now track the adapted
  `MethodType` (drop bound params / replace trailing array param) instead of copying
  the target's raw descriptor.
- Verified == HotSpot byte-for-byte via `IndyProbe.java` (MethodType param counts) and
  `GuardProbe.java` (the full guard chain): `insertArguments` pc 2→1, `asCollector` pc=2.
  Lambda/method-ref/string-concat/comparator indy smoke unchanged; 23/23 invoke tests.

**Key point:** every `type()` now matches HotSpot, yet the RUNTIME `classes[]` vs
`arguments[]` lengths still disagree. So 3c is a **runtime arg-threading / call-site
spread** issue, not a `MethodType` issue. `GuardProbe` (the isolated guard chain)
does NOT reproduce — it returns `true`. The bug only appears in the full
`make()` + `MutableCallSite` + `fromCache` path.

## Candidate root causes (in priority order)
1. **CratonVM builds the `arguments` Object[] passed to `fromCache` with the wrong
   length** — most likely missing the receiver (so `os` = N while the cached `cs` =
   N+1). `fromCache`'s `arguments` is produced by Groovy's `makeFallBack` MH chain
   (an `asCollector`/spread of the call-site invocation args). Check how CratonVM
   invokes a bootstrapped indy `CallSite`/`MutableCallSite` and how it spreads the
   operand-stack args (receiver + method args) into that `Object[]`. If the receiver
   is dropped there, `os` is one short.
2. **`classes[]` is built one too long** — Groovy's `make` builds `classes` from a
   MethodType's `parameterArray()`. If CratonVM hands `make` a type that includes the
   receiver where Groovy's own bookkeeping does not (or vice-versa), `cs` is N+1 while
   `os` is N. Cross-check which exact MethodType `make` reads for `classes` vs the
   `asCollector(Object[].class, N)` count `N`.
3. **`guardWithTest` runtime dispatch** (`MH_KIND_*` for guardWithTest in
   `native-builtins/src/lang_invoke.rs`) threads args to the guard vs the target
   inconsistently — verify the guard receives the SAME arg vector (including receiver)
   as the target.

## How to reproduce / debug
```bash
CV=C:/craton/CratonVM-sbrepos/target/release/cvsbrepos.exe   # has layers 1,2,3,3b
JDK="C:/Program Files/Java/jdk-25"
cd apps/spring-boot/buildSrc; CP="runner;$(cat test-classpath.txt)"
# Full repro (~89s, --nojit avoids the slow JIT parse):
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$CV" --java-home "$JDK" --nojit -cp "$CP" \
  RunJUnit org.springframework.boot.build.groovyscripts.SpringRepositoriesExtensionTests
# Type/guard probes (both now == HotSpot — use to confirm no type() regression):
"$CV" --java-home "$JDK" --nojit -cp "$CP" IndyProbe
"$CV" --java-home "$JDK" --nojit -cp "$CP" GuardProbe
```
**Recommended next probe:** write a Groovy-free Java repro of
`guardWithTest(insertArguments(sameClasses,0,classes).asCollector(Object[],N).asType(t),
target, fallback)` and invoke it through a `MutableCallSite.dynamicInvoker()` with a
receiver + arg, asserting `classes.length == collectedArgs.length`. That isolates
whether the call-site **invocation** (not the guard construction, which `GuardProbe`
already proves correct) drops the receiver. Decompile `IndyInterface.make` /
`makeFallBack` (groovy-4.0.29.jar) to see exactly which MethodType feeds `classes`
vs the collector count.

## Tools / artifacts
- Probes: `apps/spring-boot/buildSrc/runner/{IndyProbe,GuardProbe}.java` (committed-ish;
  the buildSrc tree is gitignored — copy out if needed).
- Binary with all 4 fixes: `C:\craton\CratonVM-sbrepos\target\release\cvsbrepos.exe`
  (worktree `CratonVM-sbrepos`, branch `fix/springrepos-coldpath`).
- The combinator code: `native-builtins/src/lang_invoke.rs`
  (`alloc_method_handle`, `bindTo`, `insertArguments`/`asCollector` registrations,
  `MH_KIND_COLLECT`/`MH_KIND_INSERT` dispatch arms, the new `mh_type_descriptor`/
  `split_descriptor_params` helpers).
