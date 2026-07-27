# `@ForkedClassPath` + `Mockito.verify()` → `NotAMockException`: `Method.invoke` resolved a cross-package package-private declaring class **by name** — FIXED

**Status: FIXED — 2026-07-26/27.** Worktree
`CratonVM-forkedclasspath-delegation-20260726`, branch
`fix/forkedclasspath-parent-delegation-20260726`, final binary
`cratonvm-forkcp-final.exe` (branched from `dev` `95e4d9929`, merged up to
`dev` `57c89f2de`).

Closes the long-standing OPEN doc
`docs/known-issues/springboot/tomcatservletwebserverservletcontextlistenertests-mockito-forkedclasspath-mockmethodadvice.md`
(found 2026-07-23, root cause narrowed twice but never fixed) **and** its
`module/spring-boot-jetty` sibling.

| Module | Class | Before | After |
|---|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests` | FAIL 2/2 | **PASS 2/2** |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebServerServletContextListenerTests` | FAIL 2/2 | **PASS 2/2** |

Verified with JIT **on** and `--nojit`. Both classes also pass on real HotSpot
under the identical harness (baseline
`.suite/baseline/hotspot-baseline-forkcp-hotspot-20260726.tsv`), so this was a
genuine CratonVM divergence.

## Symptom

```
org.mockito.exceptions.misusing.NotAMockException:
Argument passed to verify() is of type ServletContextListener$MockitoMock$3LgK7p6K and is not a mock!
	at org.mockito.internal.exceptions.Reporter.notAMockPassedToVerify(Reporter.java:151)
	at org.mockito.internal.MockitoCore.verify(MockitoCore.java:158)
	at org.mockito.Mockito.verify(Mockito.java:2946)
	at org.mockito.BDDMockito$ThenImpl.should(BDDMockito.java:284)
	at org.springframework.boot.web.server.servlet.AbstractServletWebServerServletContextListenerTests.servletContextListenerBeanIsCalled(...:69)
```

The mock is genuinely created (the message names the real generated
`$MockitoMock$` class) and its `contextInitialized` call is genuinely
recorded — only the later `verify()` cannot find it.

Note the Tomcat class's *original* 2026-07-23 symptom
(`NoClassDefFoundError: org.mockito.internal.creation.bytebuddy.MockMethodAdvice`)
no longer occurs on current `dev`; it was fixed by intervening work. As of
2026-07-26 both classes exhibit the single `NotAMockException` symptom the
Jetty sibling always had, so the two "different symptoms" in the old doc are
now one bug.

## Root cause

**`native_method_invoke` (`native-builtins/src/lang_class.rs`) resolved the
declaring class of a private / cross-package package-private instance method
through the loader-blind global name lookup, discarding the exact `ClassId`
it already held.**

`Method.invoke` has three dispatch branches:

| Case | Dispatch |
|---|---|
| ordinary instance method | virtual on the receiver's runtime class |
| `private`, or package-private whose declaring class is in a *different* package than the receiver (`crosses_package`) | `invoke_special` — must **not** be virtually retargeted (JLS §8.4.8.1) |
| `static` / `<init>` | direct on the declaring class |

The static branch had already been fixed to prefer the `Method` mirror's own
`ClassId` (`ctx.invoke_by_class_id`) precisely because name-based resolution
is unsound once two loaders define the same binary name. The
`private`/`crosses_package` branch still called
`ctx.invoke_special(&class_name, …)` — **by name**.

That is exactly the shape of these tests:

```java
// org.springframework.boot.web.server.servlet
public abstract class AbstractServletWebServerServletContextListenerTests {
    @Test @ForkedClassPath
    void servletContextListenerBeanIsCalled() { … }        // package-private!
}

// org.springframework.boot.tomcat.servlet   ← DIFFERENT package
class TomcatServletWebServerServletContextListenerTests
        extends AbstractServletWebServerServletContextListenerTests { … }
```

so `crosses_package == true`, and JUnit's reflective invocation of the test
method took the `invoke_special`-by-name path.

`ModifiedClassPathClassLoader` (Spring Boot's `@ForkedClassPath` mechanism)
correctly defined its **own** copy of both test classes, and CratonVM's
loader-faithful machinery got the fork's constructor right — but the
name-based `invoke_special` handed back the **application-loader** copy of the
abstract base, so the test *body* executed application-loader bytecode against
a fork-loaded receiver. Every constant-pool reference inside that method then
resolved in the application namespace.

Confirmed via a temporary `CRATONVM_DBG_FORKCP` trace, which showed the split
precisely:

```
invokestatic owner=org/mockito/Mockito.mock
  from=…AbstractServletWebServerServletContextListenerTests$ServletContextListenerBeanConfiguration
  from_loader=Some(UserDefined(3))     ← the @Bean factory ran in the FORK

invokestatic owner=org/mockito/BDDMockito.then
  from=…AbstractServletWebServerServletContextListenerTests
  from_loader=Some(Application)        ← the test body ran in the APPLICATION loader
  isolated=false has_user_def_loader=false dispatch=None
```

`dispatch=None` is the giveaway: because the *referencing* class was the
application copy, no loader-local owner existed, so the `invokestatic` owner
fell through to the global store and a second, application-loader copy of the
whole Mockito graph was built.

The failure then follows mechanically:

* `mock()` (called from the fork's own nested `@Configuration` class) ran the
  **fork's** `MockUtil`, registering the mock in that copy's static
  `mockMakers` map (`MockUtil.defaultMockMaker` →
  `InlineDelegateByteBuddyMockMaker.mocks`).
* `verify()` ran **application-loader** `MockitoCore`, whose
  `new DefaultMockingDetails(mock)` → `MockUtil.isMock` →
  `getMockHandlerOrNull` consulted the **application** copy's separate,
  never-populated `mockMakers` map.
* `MockitoCore.verify`'s first check (`mockingDetails(mock).isMock()`) fails →
  `Reporter.notAMockPassedToVerify`.

Every individual mechanism the old doc had exhaustively cleared (identity
hash, `WeakReference` clearing, `map_hash_key`, the `ConcurrentHashMap`
bucket walk, `WeakKey`/`LatentKey` `equals`, Spring's `MockResolver` SPI) was
genuinely correct. They were simply being asked about two disjoint maps.

### What the old doc got wrong

The 2026-07-25 update named
`url_classloader_isolated_from_app`'s gate in `cl_real_load_class_base`
(`native-builtins/src/classloader_real.rs`) as "the actual defect", on the
premise that a bare `@ForkedClassPath` loader should delegate `org.mockito.*`
to its parent and share one copy.

**That premise is wrong.** `ModifiedClassPathClassLoader.compute()` builds the
loader with `classLoader.getParent()` as its parent — i.e. the **platform**
loader, not the application loader:

```java
return new ModifiedClassPathClassLoader(processUrls(extractUrls(classLoader), annotations),
        excludedPackages(annotations), classLoader.getParent(), classLoader);
```

So on real HotSpot the fork loader genuinely *is* isolated from application
entries and genuinely *does* define its own copy of every non-excluded class
its (complete) URL list can satisfy, `org.mockito.*` included. CratonVM's
short-circuit there is HotSpot-faithful; the suggested "fix candidate" would
have been a regression. The real defect was the mirror image — fork-executed
code *leaking into* the application namespace — and it was not in
`cl_real_load_class_base` at all.

## Fix

Three small, additive changes:

1. **`native-api/src/registry.rs`** — new `NativeContext` method
   `invoke_special_by_class_id(class_id, class_name, method_name, descriptor,
   args)`, mirroring the existing `invoke_by_class_id`. Default implementation
   forwards to the name-based `invoke_special`, so no other `NativeContext`
   impl (mocks, tests) needs to change.

2. **`vm/src/vm/vm_exec.rs`** — `NativeContextImpl::invoke_special_by_class_id`
   forwards to the existing `invoke_special_shared_on_class`.

3. **`native-builtins/src/lang_class.rs`** — the `is_private || crosses_package`
   branch of `native_method_invoke` now uses
   `mirror_class_id(ctx, declaring_mirror)` when available and only falls back
   to the name-based call when the mirror carries no `ClassId` — the same
   pattern the static branch immediately below already used.

Behaviour is unchanged whenever there is only one class per binary name (the
overwhelming common case): the supplied `ClassId` is what the name lookup
would have returned anyway. It only diverges when a second same-named class
exists — which is precisely when the old path was wrong.

### Relationship to `f16acca12` (landed on `dev` during this work)

This fix was originally written against `dev` `95e4d9929` and added its own
`invoke_special_shared_resolved(Option<ClassId>, …)` split in `vm_exec.rs`.
While it was being verified, `dev` independently landed
`f16acca12 fix(jit): resolve invokespecial through the caller's loader, not the
global name map`, which introduced **the same primitive** under different
names — `invoke_special_shared_on_class` / `invoke_special_shared_impl(pre_resolved: …)`.

On merging `origin/dev` (144 commits, `57c89f2de`) the duplicate was dropped
in favour of upstream's, so this change now only *reuses* it. The two fixes are
complementary and neither subsumes the other:

* `f16acca12` fixes the **JIT** invokespecial call-site path
  (`vm/src/jit/helpers.rs` is its only caller).
* This fixes the **reflective `Method.invoke`** path, which is interpreted —
  hence the bug reproducing identically under `--nojit`.

**Negative control (empirical, not inferred):** the merged tree was built twice,
differing *only* in the `lang_class.rs` call site.

| Binary | Tomcat class | Jetty class |
|---|---|---|
| merged `dev` + all of this branch **except** the call site | FAIL | FAIL |
| merged `dev` + the call site | **PASS** | **PASS** |

## Ruled out along the way

* **Not JIT-related** — reproduces identically under `--nojit`.
* **Not a livelock/hang** — the class runs to completion in ~20 s (the
  2026-07-18 `InterceptingExecutableInvoker` livelock fix is still in effect).
* **Not the `WeakConcurrentMap` `equals` bridges** — `native_mockito_weak_key_equals`
  / `native_mockito_latent_key_equals` (added 2026-07-25, `native-builtins/src/reference.rs`)
  are a genuine, correct fix for a real dispatch defect and are kept, but they
  were never sufficient: the two maps involved were different objects.
* **Not reproducible standalone** — a hand-built `URLClassLoader` mirroring
  `ModifiedClassPathClassLoader` exactly (platform parent, full URL list, TCCL
  set, Mockito pre-initialised under the app loader) passes on both HotSpot and
  CratonVM. Reproducing needs a *package-private test method inherited across
  packages* invoked reflectively — the piece none of the earlier standalone
  repros had. Kept at `C:\craton\forkrepro-20260726` for reference.

## How to re-derive the trace

The `CRATONVM_DBG_FORKCP` instrumentation was removed before commit (its name
filters were specific to this investigation). To rebuild it, add a
`cached_is_ok!(dbg_forkcp, "CRATONVM_DBG_FORKCP")` to
`vm/src/runtime/env_cache.rs` plus a `dbg_forkcp` field on
`types/src/flags.rs`'s `LoaderFlags`, then print at:

* `class_manager.rs::define_class_shared_with_options` — name + `loader_id`
  (which loader defined each copy),
* `interpreter.rs::resolve_class_loader_aware` — name, referencing class +
  loader, `isolated`, `known`, `user_loader`, `driven`,
* `interpreter.rs::execute_invokestatic` after `static_dispatch_class_id` —
  owner, referencing class + loader, `dispatch_loader` (**this is the one that
  localises the bug**),
* `classloader_real.rs::cl_real_load_class_base` — which delegation branch
  each name took.

`CRATONVM_DBG_DUPCLASS=1` (already on `dev`) is a fast first probe: it prints
every time `resolve_fast_path_class_id` rejects an existing
`UserDefined`-loader candidate and mints a separate `Application` `ClassId`.
Pre-fix that fired for ~100 `org.mockito.*` classes; post-fix `MockUtil` has
no application copy at all.

## Residual observations (not fixed, not fatal here)

The pre-fix trace showed a handful of *other* Mockito classes acquiring an
application-loader copy while the fork was running — `PremainAttach`,
`ModuleHandler`/`ModuleHandler$ModuleSystemFound`, `Java8LocationImpl` +
`Location`, `ReflectionMemberAccessor` + `MemberAccessor`, `MockAccess`. Each
appears as a subtype+supertype pair defined globally right after the fork
defined the subtype, with no `resolve_class_loader_aware` or `loadClass`
trace — i.e. some other path reaching `load_class_concurrent` directly. These
persist post-fix (11 application-loader `org.mockito.*` defines remain, down
from ~104) and do **not** affect these tests, but they are the obvious next
thread to pull if another fork-loader identity bug surfaces.

## Regression check

### Loader-isolation family (the at-risk set) — zero change

Every Spring Boot test class carrying `@ForkedClassPath`, `@ClassPathExclusions`,
`@ClassPathOverrides` or `@CompileWithForkedClassLoader` that the suite runner
can execute — **90 classes** — run on the pre-fix `dev` binary and the fixed
binary, same harness, same parallelism:

| | PASS | FAIL |
|---|---:|---:|
| pre-fix (`dev` `95e4d9929`) | 67 | 23 |
| fixed | 67 | 23 |

A class-by-class diff of *(status, failed-test count)* across all 90 is
**byte-identical** — not merely the same totals. The 23 failures are
pre-existing, unrelated `dev` issues (logging systems, AOT processors, JPA,
JTA, WebSocket messaging, …), each already tracked elsewhere.

Class list generation:

```bash
grep -rl "@ForkedClassPath\|@ClassPathExclusions\|@ClassPathOverrides\|@CompileWithForkedClassLoader" \
  --include=*.java apps/spring-boot | grep "/src/test/java/"
```

Kept at `C:\craton\forkrepro-20260726\loaderiso-regress.tsv`.

### Broad sweep — 583 classes, zero regressions

`core/spring-boot` + `core/spring-boot-test` + `core/spring-boot-autoconfigure`
+ `module/spring-boot-tomcat` + `module/spring-boot-jetty` +
`module/spring-boot-web-server`, on the final merged binary
(`.suite/results/broad-FIX-20260727/`):

| PASS | FAIL | HANG | EMPTY |
|---:|---:|---:|---:|
| 559 | 12 | 2 | 10 |

`EMPTY` is a harness artifact, not a failure: those 10 are abstract base test
classes (`AbstractPropertyMapperTests`, `AbstractJsonParserTests`,
`AbstractLoggingSystemTests`, …) that declare no runnable tests of their own.

Every one of the 14 non-passing classes was then re-run on the **negative-control
binary** — the same merged tree built without only the `lang_class.rs` call site
(`.suite/results/broad-NEGCTL-20260727/`). A diff of *(status, failed-test count)*
across all 14 is **identical**, including both HANGs
(`JettyServletWebServerFactoryTests`, `TomcatServletWebServerFactoryTests` — the
already-documented Xerces/TLD-scan throughput wall, which needs ~800-1100s and
exceeds the 420s sweep budget).

Since a regression could only appear as a class that fails *with* the fix, and
every such class fails identically *without* it, the sweep shows no regression
attributable to this change.

### Both target classes

PASS with JIT **on** and `--nojit`, on the pre-merge fixed binary, the
instrumentation-free binary, and the final merged binary.
