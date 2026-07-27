# Infinispan JGroupsTransport.start() never invoked (was misdiagnosed as a Netty setAccessible bug) — FIXED

Status: FIXED (2026-07-06). Moved out of `../../../known-issues` per the
known-issues triage rule — see
`docs/known-issues/keycloak-model-infinispan-cache-config-null-after-real-start.md`
for the residual that surfaced once this was fixed.

Date observed: 2026-07-06 (original misdiagnosis); root-caused and fixed
2026-07-06 same day.

## Corrected summary

The original title/diagnosis of this doc was **wrong** — confirmed via direct
bytecode-level investigation (decompiling the real Netty jars with `javap`)
and empirical A/B testing against real JDK 25 on the same classpath. There is
**no Netty/setAccessible bug.**

The real defect: **Infinispan's DI container (`BasicComponentRegistryImpl`)
never called `.start()` on the `JGroupsTransport` component**, because
`DefaultCacheManager.start()`/`.stop()` were natively shimmed
(`native_dcm_start`/`native_dcm_stop` in `../../../../native-builtins/src/infinispan_local.rs`,
part of the legacy "T19.10" synthetic local-cache backend) *unconditionally*
— native dispatch is keyed by class+method+descriptor, not by which
constructor built the receiver, so the shim also intercepted `.start()`/
`.stop()` on a REAL `DefaultCacheManager` built via the un-shimmed
`(ConfigurationBuilderHolder, boolean)` constructor (the overload Keycloak's
own `DefaultInfinispanConnectionProviderFactory` actually uses). The
component *was* correctly instantiated and wired (its fields —
`configuration`, `marshaller`, `notifier`, etc. — were populated via the
generated `CorePackageImpl$2.wire(...)` accessor, confirmed by interpreter
tracing), but the shimmed `.start()` call ran the synthetic
`global_manager().start()` against an unrelated process-wide singleton
instead of the real object's own `internalStart(boolean)` (which starts
`GlobalComponentRegistry` — module lifecycles, JGroups transport, etc.).
That left `JGroupsTransport.channel` permanently null, surfacing later as an
NPE in `DefaultInfinispanConnectionProviderFactory.createEmbeddedCacheManager`
when it called `.getChannel().getProtocolStack()`.

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

Every call site in Netty checks the returned `Throwable` for `null`/
`instanceof` and degrades gracefully — it is never rethrown. Verified this
behaves identically on real JDK 25 and CratonVM across many scenarios.

## Root cause, confirmed via interpreter tracing

Interpreter-level PC/entry tracing (temporary instrumentation in
`../../../../vm/src/runtime/interpreter.rs`, not committed) proved:

- `JGroupsTransport`'s constructor and its DI accessor's `wire(...)` bridge
  both correctly ran (fields populated).
- `CorePackageImpl$2.start(Object)` / `start(JGroupsTransport)` — which would
  call the real `JGroupsTransport.start()` — were **never entered at all**.
- Tracing up the call chain: `DefaultCacheManager.start()` itself (invoked
  via `invokevirtual` at the tail of its constructor, `if (start) start()`)
  never entered real bytecode either — it went straight from the
  `invokevirtual` instruction to the constructor's `return`, confirming a
  **native override intercepting the call**.
- Found the culprit: `../../../../native-builtins/src/infinispan_local.rs` registers
  `native_dcm_start`/`native_dcm_stop` on `DefaultCacheManager` unconditionally.

## The fix

`../../../../native-builtins/src/infinispan_local.rs`: added `is_real_dcm()`, which
distinguishes a real object from a synthetic one by checking whether
`globalComponentRegistry` (a field only the real constructor ever sets) is
non-null. `native_dcm_start`/`native_dcm_stop` now check this and, for a
real object, delegate directly to the real `internalStart(boolean)`/
`internalStop()` via `ctx.invoke_virtual` (bypassing the natively-overridden
`start()`/`stop()` bytecode itself, which would just re-enter the native).

**Field-0/`DCM_FIELD_HANDLE`-based discrimination doesn't work**: tried
first, but `native_dcm_init`'s `Value::Long` write into what real bytecode
declares as a reference-typed field (`caches`) doesn't round-trip reliably
— the class's synthetic 3-slot layout registration overrides field-type
metadata for its first few indices, so a zero-initialized reference field
there reads back as `Value::Int(0)` instead of `Value::Object(None)`,
indistinguishable from other cases. `globalComponentRegistry` sits well past
that range and reads back reliably.

Two more small native-method gaps were found and fixed while verifying the
fix against the real `RealmModelTest` (each was blocking further progress,
one layer at a time, once the primary fix let the real JGroups channel-setup
code actually run for the first time):

- `../../../../native-io/src/lib.rs`: `sun/nio/ch/UnixDispatcher.close0(Ljava/io/FileDescriptor;)V`
  was never registered (only `.init()` was, as a no-op) — real
  `MulticastSocket` close hit `UnsatisfiedLinkError`. Reused the existing
  `native_fd_close0` handler (same calling convention: `args[0]` is the
  `FileDescriptor`).
- `../../../../native-io/src/lib.rs`: `sun.nio.ch.NativeSocketAddress`'s 12 native
  probes (`AFINET()`, `sizeofSockAddr4()`, `offsetSin6Addr()`, etc.) were
  entirely unregistered. These are fixed platform ABI constants (struct
  layout for `sockaddr_in`/`sockaddr_in6`), so registered them reading
  straight off Rust's own `libc::sockaddr_in`/`sockaddr_in6` via
  `std::mem::size_of`/`std::mem::offset_of!` rather than hardcoding
  platform-specific magic numbers.

## Verification

- `ISPNProbe3.java` (`new DefaultCacheManager(holder, true)`, matching
  Keycloak's exact call shape): `channel` now correctly non-null; JGroups
  channel connects (`local_addr: node-1...`, `I'm the first member: creating
  cluster as coordinator`, matching real JDK 25's log output exactly).
- `SyntheticRegressionProbe.java` (`new DefaultCacheManager()` +
  `getCache()`/`put`/`get`/`remove`, the OLD synthetic-shim path used
  elsewhere): confirmed still works correctly — no regression.
- Full `RealmModelTest` via `KcRunner`: now gets past `createEmbeddedCacheManager`
  entirely (JGroups channel connects, cluster view received) — the
  originally reported NPE is gone. It now fails at a **different, deeper**
  point (`GlobalConfigurationManagerImpl.postStart()` → `cache.config` null)
  — a separate, distinct bug in the SAME "synthetic native shim intercepts a
  real object" family, now tracked at
  `docs/known-issues/keycloak-model-infinispan-cache-config-null-after-real-start.md`.

Requires the *real* Keycloak-resolved dependency versions (Infinispan
16.0.8, Netty 4.1.132.Final, JGroups 5.5.1.Final) to reproduce/verify — the
older WildFly-bundled versions (14.0.28/4.1.108/5.2.25) never exercised this
code path at all.
