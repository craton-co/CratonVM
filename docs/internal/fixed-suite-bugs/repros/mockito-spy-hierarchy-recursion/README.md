# Mockito `spy()` cross-hierarchy real-method recursion — repro kit

Companion to
[`CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md`](../../known-issues/CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md)'s
`context.annotation.ImportSelectorTests` entry (2026-07-13 investigation).
`StackOverflowError` on 5/9 sub-tests, all and only the ones that call
`Mockito.spy(new DefaultListableBeanFactory())`. Nothing to do with Spring's
`ImportSelector`/`ConfigurationClassParser` recursion — the original doc's
hypothesis was wrong.

## Repro chain (fastest signal first)

- **`SpyDLBFProbe.java`** — the tightest standalone repro. No Spring context,
  no JUnit: `spy(new DefaultListableBeanFactory())` then ONE
  `spy.registerSingleton("x", "y")` call. Reproduces the identical
  `StackOverflowError` in ~2 minutes real time on this session's build
  (`cratonvm-importselector-local.exe`), with `--nojit` making no
  difference (rules out a JIT miscompile — same failure, same rough timing,
  interpreter-only).
- **`OverrideProbe.java`** — plain-reflection sanity check, no Mockito at
  all: does `Class.getMethod()`/`getDeclaringClass()` correctly identify
  that `DefaultListableBeanFactory` overrides
  `DefaultSingletonBeanRegistry.registerSingleton`? **Confirmed correct**,
  byte-for-byte matching HotSpot, both before *and* after the class
  hierarchy has been retransformed by Mockito (run this after `spy()` too —
  see `SpyThenGraphProbe.java`).
- **`SpyThenGraphProbe.java`** — calls ByteBuddy's own
  `MethodGraph.Compiler` (the exact algorithm
  `MockMethodAdvice.isOverridden()` uses) directly against the
  POST-RETRANSFORM `spy.getClass()`. **Confirmed correct**: representative
  method for `registerSingleton` resolves to `DefaultListableBeanFactory`,
  matching HotSpot. Also confirmed a real, *separate* performance
  finding: this computation takes **~70s** on CratonVM for a
  freshly-retransformed class vs. instant for a normal class — not the
  cause of the infinite recursion (the recursion's own per-iteration cost is
  fast; total repro time ~139s ≈ 70s one-time `MethodGraph` compute + ~69s
  of many fast recursive frames before the stack is exhausted) but worth
  filing separately if anyone revisits Instrumentation/retransform
  performance.
- **`SelfCallProbe.java`** — isolates Mockito's actual guard mechanism in
  miniature: a `ThreadLocal<Object>` storing an object reference
  (`replace()`), a forced GC in between (`System.gc()` ×2 after allocating
  ~150MB of garbage), then a `==` comparison against a fresh reference to
  the same logical object (`checkSelfCall()`). **Confirmed correct** —
  CratonVM's ThreadLocal-value GC-safety (`native_tl_get`/`native_tl_set` in
  `../../../../../native-builtins/src/phases_early.rs`, using `add_global_root`/
  `resolve_global_root`) handles this fine; the stored reference is
  correctly forwarded across the GC.

## Root-cause chain, decompiled from the actual mockito-core 5.23.0 /
byte-buddy 1.18.3 jars (`javap -p -c`, not guessed from memory)

`Mockito.spy(existingInstance)` on a non-final class uses the **inline**
mock maker's retransformation path (`Instrumentation.retransformClasses`),
NOT subclassing — confirmed via `-Dnet.bytebuddy.dump=<dir>`: the receiver's
runtime class stays `DefaultListableBeanFactory` (unchanged identity), and
**every class in the hierarchy** gets its bytecode rewritten in place
(`DefaultListableBeanFactory`, `AbstractAutowireCapableBeanFactory`,
`AbstractBeanFactory`, `FactoryBeanRegistrySupport`,
`DefaultSingletonBeanRegistry`, `SimpleAliasRegistry`, plus interfaces).

Each redefined method's new bytecode is (decompiled from the dump):
```
Object m2pTHxR6 = MockMethodDispatcher.get(identifier, this);
if (m2pTHxR6 != null && m2pTHxR6.isMocked(this) && !m2pTHxR6.isOverridden(this, thisMethod)) {
    Callable<?> c = m2pTHxR6.handle(this, thisMethod, args);
    if (c != null) return c.call();
}
/* ...original method body, unmodified... */
```

`isOverridden()` compiles ByteBuddy's own `MethodGraph` for the receiver's
class and returns `true` when a MORE-DERIVED class than `thisMethod`'s
declaring class provides the actual dispatch target — i.e. "skip
interception here, the real entry point for this call is the override,
which will (or already did) handle it." **Confirmed correct on CratonVM**
(see `OverrideProbe`/`SpyThenGraphProbe` above).

`isMocked()` is:
```
boolean isMocked(Object instance) {
    if (isMock(instance) || getSingletonMockInterceptor(instance) != null) {
        return selfCallInfo.checkSelfCall(instance);   // <-- THE self-call guard
    }
    return false;
}
```
`selfCallInfo` is a `MockMethodAdvice$SelfCallInfo extends ThreadLocal<Object>`
whose `checkSelfCall(Object o)` is `if (o == get()) { set(null); return false; } return true;`
— i.e. `isMocked()` returns **false** (skip interception, run the original
body unmodified) exactly when we're re-entering via a "call the real
method" reflective invocation that (unavoidably, since
`Lookup.unreflect()` on a public method dispatches virtually) lands back on
the *same* redefined method. `SerializableRealMethodCall.invoke()` (the
concrete `RealMethod` used for `Serializable` mocks — confirmed
`DefaultListableBeanFactory` takes this path from the live stack trace)
sets `selfCallInfo` via `replace(instance)` immediately before the
reflective call and restores the previous value in a `finally`.

**This is the guard that appears not to terminate the recursion on
CratonVM**, even though every input it depends on that could be tested in
isolation (plain reflection, ByteBuddy's MethodGraph, a standalone
ThreadLocal+forced-GC+`==` simulation) came back correct. The live
`KRUN_STACK=1` trace shows the cycle:

```
DefaultListableBeanFactory.registerSingleton  (line 1491, calls super)
  -> DefaultSingletonBeanRegistry.registerSingleton (line 142, advice entry)
     -> MockMethodDispatcher.handle -> ... -> InstrumentationMemberAccessor.invoke
        -> DefaultListableBeanFactory.registerSingleton  (line 1491, AGAIN)
           -> ... repeats forever
```
i.e. calling "the real method" for `DefaultSingletonBeanRegistry
.registerSingleton` keeps landing back on
`DefaultListableBeanFactory.registerSingleton`'s advice entry instead of
terminating — consistent with `isMocked()`/`checkSelfCall()` never
returning `false` for this receiver during this call chain, for a reason
this session could not pin to a single Rust source line without further
instrumentation (would need a print/log inside
`MockMethodDispatcher.get()`'s resolution — that class is bootstrap-
injected real bytecode, not a CratonVM native, so nothing to patch
directly; the next step would be adding a temporary `CRATONVM_DBG_*` hook
around ThreadLocal get/set specifically filtered to `SelfCallInfo`-shaped
keys, mirroring the existing note in
`http-client-simpleclienthttpresponsetests-mockito-dispatch-bugs-FIXED.md`
for `ThreadSafeMockingProgress$1`).

## Leading unconfirmed hypothesis for the next session

`MockMethodDispatcher.get(identifier, instance)` is the bootstrap-injected
static bridge that every redefined method (across ALL classes in the
hierarchy) and `SerializableRealMethodCall.invoke()` independently call to
reach the ONE shared `MockMethodAdvice` instance (and hence its ONE
`selfCallInfo` ThreadLocal object). If CratonVM's handling of this
bootstrap-appended, dynamically-injected class results in more than one
live `Class`/static-state instance being reachable from different call
sites (e.g. one copy per redefined class's defining loader context, rather
than a single canonical bootstrap-loaded copy), each would carry its own
distinct `selfCallInfo` object, and the guard would never see a matching
`ThreadLocal` between the "set" and "check" call sites — exactly explaining
unconditional non-termination without needing any single check to return
a "wrong" boolean. This was not directly instrumented/confirmed this
session (would need to print `System.identityHashCode(MockMethodDispatcher
.get(...))` from within a temporarily-patched redefined method, or a
CratonVM-side trace of every `Class` object minted for the name
`org.mockito.internal.creation.bytebuddy.inject.MockMethodDispatcher`).

## Why this is unrelated to the rest of the 1500s-cluster doc

None of the other clusters in that doc involve Mockito `spy()` (as opposed
to plain `mock()`, which bug-09
(`docs/internal/kafka-suite-bugs/bug-09-mockito-inline-mockmaker-selfattach.md`)
and `spring-boot-groovy-indy-mockito-mock-dispatch.md` already verified
working end-to-end). `spy()`'s `CALLS_REAL_METHODS` default answer is the
first workload in this codebase's history to exercise Mockito's
cross-hierarchy "call the real method, skip re-interception" path at all —
`mock()`'s default answer (return null/0/false) never invokes real method
bodies, so this path was never exercised by any of the prior, now-fixed
Mockito work.
