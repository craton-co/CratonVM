# Infinispan JGroupsTransport.start() never invoked (was misdiagnosed as a Netty setAccessible bug)

Status: open

Date observed: 2026-07-06 (original misdiagnosis); root-caused 2026-07-06 same day.

## Corrected summary

The original title/diagnosis of this doc was **wrong** — confirmed via direct
bytecode-level investigation (decompiling the real Netty jars with `javap`)
and empirical A/B testing against real JDK 25 on the same classpath. There is
**no Netty/setAccessible bug.**

The real defect, confirmed with a precise repro matching Keycloak's actual
code path: **Infinispan's DI container (`BasicComponentRegistryImpl`) never
calls `.start()` on the `JGroupsTransport` component.** The component *is*
correctly instantiated and wired (its fields — `configuration`, `marshaller`,
`notifier`, etc. — are populated via the generated
`CorePackageImpl$2.wire(...)` accessor, confirmed by interpreter tracing),
but the subsequent lifecycle callback that would call
`JGroupsTransport.start()` (which connects the JGroups channel and assigns
`this.channel`) is silently skipped — no exception, no log — leaving
`channel` permanently null. When Keycloak later calls
`((JGroupsTransport) transport).getChannel().getProtocolStack()...`, that's
a `NullPointerException`.

### Why the original diagnosis was wrong

`io.netty.util.internal.ReflectionUtil.trySetAccessible(AccessibleObject, boolean)`
(decompiled from the real `netty-common` jar used by Keycloak's Infinispan
dependency, 4.1.132.Final) does contain the literal string
`"Reflective setAccessible(true) disabled"` — but it's **returned as a value**,
never thrown:

```java
public static Throwable trySetAccessible(AccessibleObject o, boolean checkAccessible) {
    if (checkAccessible && !PlatformDependent0.isExplicitTryReflectionSetAccessible()) {
        return new UnsupportedOperationException("Reflective setAccessible(true) disabled");
    }
    try { o.setAccessible(true); return null; }
    catch (SecurityException e) { return e; }
    catch (RuntimeException e) { return handleInaccessibleObjectException(e); }
}
```

Every one of its ~4 call sites in Netty (`PlatformDependent0$1/$5/$6`,
`NioEventLoop$4`) checks the returned `Throwable` for `null`/`instanceof` and
degrades gracefully (logs "unavailable", moves on) — it is never rethrown.
This behaves **identically** on real JDK 25 and CratonVM (verified with
standalone repros: bare `PlatformDependent0` class-load, direct
`trySetAccessible()` calls, `NioEventLoopGroup` construction, a plain
`JChannel` JGroups connect — all matched real JDK 25 exactly, including with
`-Dio.netty.tryReflectionSetAccessible=true` explicitly set).

The original stderr trace (`at KcRunner.main(KcRunner.java:34)` as the top
frame) was a real symptom, but of a *different*, deeper failure. Once
CratonVM's own `<clinit>`-failure diagnostics were consulted directly, the
*actual* cause is:

```text
[CLINIT] <clinit> failed — wrapping in ExceptionInInitializerError
  class=org/keycloak/testsuite/model/KeycloakModelTest
  cause=java/lang/NullPointerException Cannot invoke "org.jgroups.JChannel.getProtocolStack()"
    because the return value of "org.infinispan.remoting.transport.jgroups.JGroupsTransport.getChannel()" is null
    at org.keycloak.connections.infinispan.DefaultInfinispanConnectionProviderFactory.createEmbeddedCacheManager(DefaultInfinispanConnectionProviderFactory.java:266)
    at org.keycloak.connections.infinispan.DefaultInfinispanConnectionProviderFactory.lazyInit(...)
    ...
```

This matches the original bug's reported `createEmbeddedCacheManager` frame
exactly — just with a completely different underlying cause.

## A red herring along the way: a stale synthetic `DefaultCacheManager` shim

Initial narrowing used `new DefaultCacheManager(globalConfig)` (the
single-arg `GlobalConfiguration` constructor) as a minimal repro, and found
that essentially *none* of `DefaultCacheManager`'s own fields (`health`,
`cacheManagerInfo`, `authorizer`, `stats`) were ever set — interpreter PC
tracing showed the constructor's bytecode **never executes at all**. Root
cause: `native-builtins/src/infinispan_local.rs` ("T19.10 — Infinispan
local-mode cache natives") registers a **native override** for exactly this
constructor overload —

```rust
registry.register(CLS_MANAGER, "<init>", "(Lorg/infinispan/configuration/global/GlobalConfiguration;)V", native_dcm_init);
```

— a legacy synthetic local-cache backend (predating the later migration of
`ConfigurationBuilder`/`GlobalConfigurationBuilder` to real bytecode,
documented in the same file's module comment and in
[[reference_infinispan_identity_wrapper_natives]]). It sets only a 3-slot
synthetic layout (`handle`, `config_name`, `started`) and never touches the
real Infinispan object graph at all — so any *real* getter that reads a real
field (`getHealth()`, `getStats()`, etc.) sees whatever was there at
allocation time: null.

**This is a real, harmless-so-far but real gap** — but it is NOT what causes
the original bug. Keycloak's own code
(`DefaultInfinispanConnectionProviderFactory.getDefaultCacheManager`) calls
`new DefaultCacheManager(holder, true)` — the
`(ConfigurationBuilderHolder, boolean)` overload — which is **not** in the
native registry's list (only the zero-arg, `(GlobalConfiguration)`, and
`(GlobalConfiguration, Configuration)` overloads are registered). That
overload runs genuine real bytecode (confirmed: real `DefaultCacheManager.java`
line numbers appear in stack traces, and `getHealth()`/`getCacheManagerInfo()`
correctly return non-null when this overload is used — see `ISPNProbe3.java`
repro below).

## Root cause, confirmed with the correct constructor overload

`ISPNProbe3.java` (below) builds a `ConfigurationBuilderHolder` and calls
`new DefaultCacheManager(holder, true)`, matching Keycloak's exact call
shape. Under CratonVM:

```text
cm created: class org.infinispan.manager.DefaultCacheManager
health=org.infinispan.health.impl.HealthImpl@...          <- correctly non-null
cacheManagerInfo=org.infinispan.manager.CacheManagerInfo@...  <- correctly non-null
transport=org.infinispan.remoting.transport.jgroups.JGroupsTransport@...
channel=null                                               <- BUG
```

Interpreter-level tracing (temporary `eprintln!` instrumentation in
`vm/src/runtime/interpreter.rs`'s main dispatch loop, gated by a throwaway
env var, removed before commit — reproducible by re-adding a
class/method-name filtered trace at the top of `execute_frame`'s loop)
showed:

- `JGroupsTransport`'s constructor runs (`<init>()V`, `<init>(NodeVersion)V`).
- Its DI accessor, the annotation-processor-generated
  `org.infinispan.remoting.transport.jgroups.CorePackageImpl$2`, correctly
  dispatches `wire(Object,...)` → `wire(JGroupsTransport,...)` (the
  type-specific override) — confirmed entered and populating `configuration`,
  `marshaller`, `notifier`, `timeService`, `invocationHandler`,
  `timeoutExecutor`, `nonBlockingExecutor`, `jmxRegistration`,
  `metricsManager`, `telemetry` via ordinary `putfield`.
- `CorePackageImpl$2.start(Object)` / `start(JGroupsTransport)` — the bridge
  and type-specific override that would call
  `JGroupsTransport.start()` (which builds/connects the JGroups channel and
  assigns `this.channel`) — **are never entered at all**, confirmed by
  grepping the entire execution trace for this exact class+method
  combination: zero hits, despite `wire(...)` on the *same* accessor object
  being entered twice.

So this is **not** a virtual-dispatch/bridge-method bug (dispatch clearly
works correctly for `wire()` on this exact object) — it's that
`BasicComponentRegistryImpl` never even attempts to call `.start()` on this
component's accessor.

Decompiling `BasicComponentRegistryImpl.doStartWrapper(ComponentWrapper)`
found two candidate gates that would produce exactly this silent skip:

```java
private void doStartWrapper(ComponentWrapper wrapper) throws Exception {
    if (wrapper.aliasTarget != null) { wrapper.aliasTarget.running(); return; }   // gate 1
    if (wrapper.accessor == null) throw new IllegalStateException(...);
    startDependencies(wrapper);
    if (!wrapper.manageLifecycle) return;                                        // gate 2 — never reaches invokeStart()
    logStartedComponent(wrapper);
    invokeStart(wrapper.instance, wrapper.accessor);   // -> accessor.start(instance)
}
```

One of these two — `aliasTarget` being non-null when it should be null for a
"real" (non-alias) component, or `manageLifecycle` being `false` when it
should be `true` for `JGroupsTransport` (a genuine `Lifecycle` component with
real `start()`/`stop()` work) — evaluates differently under CratonVM than
under real JDK 25 for this specific component's wrapper. This must be a
CratonVM-side divergence: real JDK 25 demonstrably starts the JGroups
channel correctly for the exact same bytecode/config (see the `ISPNProbe3`
run under real java in the investigation transcript — channel connects,
"I'm the first member: creating cluster as coordinator" logged, etc.).

**Not yet pinned down**: which of the two fields is wrong, on the
`ComponentWrapper` for the `Transport`/`JGroupsTransport` component
specifically, and *why*. `manageLifecycle`/`aliasTarget` are set during
`registerComponent(...)` / lazy-instantiation (`BasicComponentRegistry.getComponent(Transport.class)`,
called from `GlobalComponentRegistry`'s own constructor purely for the
instantiation side-effect) — likely derived from the accessor's own metadata
(`scopeOrdinal`, `survivesRestarts`, alias registrations) rather than a
literal boolean passed at the call site. A follow-up session should:

1. Extend the existing `CRATONVM_DBG_FIELDADDR` field-tracing mechanism
   (`vm/src/runtime/interpreter.rs`, the `matches!(fname, "unsharedLongs" | ...)`
   allowlists in both the `Getfield`/`Putfield` handlers — already used
   during this investigation, safe to extend temporarily) to also print the
   wrapper's `name` field (a `String`) alongside `manageLifecycle`'s actual
   boolean value (not just whether it's an object — the existing trace only
   logs `valObj: bool` distinguishing object vs. primitive, not the primitive
   value itself) for every `ComponentWrapper`, to identify the exact value
   used for the `Transport` component's wrapper.
2. Compare against real JDK 25 (same trace mechanism doesn't exist there, but
   attaching a debugger or adding print statements to a local Infinispan
   checkout would show the expected values) to confirm the CratonVM value is
   actually wrong (vs. Keycloak/Infinispan's own logic legitimately deciding
   not to manage this one, with something else responsible for calling
   `start()` — considered unlikely since real JDK 25 successfully starts the
   channel through this exact path, but not yet ruled out).
3. Find how `BasicComponentRegistry.registerComponent`/the lazy-instantiation
   path (`ComponentRegistry.getComponent0` → `instantiateWrapper` →
   `wireWrapper` → sets these flags from the `ComponentAccessor`'s metadata —
   `getScopeOrdinal()`, `getSurvivesRestarts()`) computes `manageLifecycle`,
   and check each contributing value/comparison against CratonVM's handling
   (boxed-`Integer` comparison via `==`/`.equals()`, enum comparison, etc. —
   common sources of real-vs-synthetic divergence elsewhere in this
   codebase).

## Ruled out

- `CRATONVM_DISABLE_JIT=1` — reproduces identically under the pure
  interpreter (confirmed via `CRATONVM_DBG_JIT_ENTRY=1` showing **zero** JIT
  entries during the whole run). Not JIT-specific.
- `-Xmx4g -Xms4g` and `CRATONVM_MOVING_YOUNG=1` (explicit opt-in to the
  finished-but-default-off moving/compacting young-gen collector) — no
  change either way. Not GC-timing- or GC-movement-related.
- Older WildFly-bundled dependency versions (Infinispan 14.0.28.Final /
  Netty 4.1.108.Final / JGroups 5.2.25.Final) do **not** reproduce this at
  all — only the actual Keycloak-resolved versions (Infinispan 16.0.8 /
  Netty 4.1.132.Final / JGroups 5.5.1.Final) do. Use the real versions when
  reproducing.

## Repro

Requires the *real* Keycloak-resolved dependency versions. Generate the
classpath from the actual reactor:

```powershell
cd apps/keycloak
./mvnw -q -pl testsuite/model -DskipTests -Dcheckstyle.skip -Dformat.skip -Dspotbugs.skip -o `
  org.apache.maven.plugins:maven-dependency-plugin:3.6.1:build-classpath `
  -Dmdep.outputFile=cp.txt -Dmdep.includeScope=test
```

Then either run `org.keycloak.testsuite.model.RealmModelTest` via a
JUnit-Platform launcher (see `apps/keycloak-suite-runner/run-keycloak-suite.ps1`'s
`Get-ModuleSystemProperties` for the required
`-Dkeycloak.model.parameters=Infinispan,Jpa` + H2 JDBC sysprops — without
these the test fails earlier with an unrelated harness gap, see
[[reference_keycloak_testsuite_model_parameters_gap]]), or use this minimal
standalone repro (no Keycloak/JUnit needed):

```java
// ISPNProbe3.java — needs infinispan-core/infinispan-commons/
// infinispan-component-annotations/protostream(-types)/caffeine/rxjava/
// jboss-logging/jboss-threads/jgroups/netty-{common,buffer,transport,resolver,codec}/
// reactive-streams/infinispan-clustered-counter/infinispan-counter-api
// (all resolvable from the versions above) on the classpath.
GlobalConfigurationBuilder gcb = new GlobalConfigurationBuilder();
gcb.transport().defaultTransport();
ConfigurationBuilderHolder holder = new ConfigurationBuilderHolder(
    Thread.currentThread().getContextClassLoader(), gcb);
EmbeddedCacheManager cm = new DefaultCacheManager(holder, true);   // matches Keycloak's real call shape
Transport transport = GlobalComponentRegistry.componentOf(cm, Transport.class);
JChannel ch = ((JGroupsTransport) transport).getChannel();
System.out.println("channel=" + ch);   // null under CratonVM; a connected JChannel under real JDK 25
```

Currently: `FAIL` under CratonVM (`channel=null`, and — via the full
`RealmModelTest` path — a real, deterministic `NullPointerException` /
`ExceptionInInitializerError` in `KeycloakModelTest.<clinit>`, blocking all
37 `testsuite/model` classes from getting past `createEmbeddedCacheManager`).
`PASS` under real JDK 25 with the identical classpath/config (channel
connects, cluster view received, etc.). Not a VM crash — a silently-skipped
lifecycle callback.
