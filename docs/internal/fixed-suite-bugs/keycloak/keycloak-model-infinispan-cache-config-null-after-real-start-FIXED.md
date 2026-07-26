# Infinispan real Cache objects got a synthetic (fieldless) Cache from native_dcm_get_cache, causing `cache.config` NPEs — FIXED

Status: FIXED (2026-07-06). Moved out of `../../../known-issues` per the
known-issues triage rule.

Date observed: 2026-07-06 — surfaced as the residual once
[keycloak-model-jgroupstransport-start-never-invoked-FIXED](keycloak-model-jgroupstransport-start-never-invoked-FIXED.md)
was fixed. Root-caused and fixed the same day.

## Summary

Same bug *family* as the one just fixed: `../../../../native-builtins/src/infinispan_local.rs`'s
"T19.10" synthetic local-cache backend registers natives for
`DefaultCacheManager`/`Cache`/`CacheImpl`/`CacheAdvanced` (`getCache`,
`put`, `get`, `remove`, `containsKey`, `size`, `clear`, `evict`,
`addListener`, `removeListener`, `getName`, `putIfAbsent`, `replace`, etc.)
**unconditionally** — native dispatch is keyed by class+method+descriptor,
not by which constructor built the receiver. `native_dcm_start`/`stop` had
exactly this bug (already fixed via `is_real_dcm()`); `native_dcm_get_cache`
(and the `Cache`-instance natives) had it too.

Once `DefaultCacheManager.start()` correctly runs the real
`internalStart(boolean)`, Infinispan's own internal bootstrap code
(`GlobalComponentRegistry.postStart()` → `GlobalConfigurationManagerImpl.postStart()`)
iterates real, internal caches and calls `.entrySet()`/`.forEach()` on them
— reached via `SecurityActions.getCache(EmbeddedCacheManager, String)` →
`EmbeddedCacheManager.getCache(String)`, i.e. this exact native. That call
reached `AbstractCacheBackedSet.<init>`, which reads `cache.config.clustering()`
— and `cache.config` was null, because the `Cache` object it was operating
on was one `native_dcm_get_cache` fabricated: a **synthetic** object (only 3
fields ever written: `handle`/`name`/`manager`, backed by a Rust-side
`CacheInner` HashMap) even though it was allocated with the REAL `CacheImpl`
class's full field layout (so `config`, along with every other real field,
sat at its zero-initialized default — `null`).

```text
Caused by: java.lang.NullPointerException: Cannot invoke "org.infinispan.configuration.cache.Configuration.clustering()" because "cache.config" is null
    org.infinispan.cache.impl.AbstractCacheBackedSet.<init>(AbstractCacheBackedSet.java:54)
    org.infinispan.cache.impl.CacheBackedEntrySet.<init>(CacheBackedEntrySet.java:22)
    org.infinispan.cache.impl.CacheImpl.entrySet(CacheImpl.java:851)
    ...
    org.infinispan.globalstate.impl.GlobalConfigurationManagerImpl.postStart(GlobalConfigurationManagerImpl.java:128)
    org.infinispan.CoreModule.cacheManagerStarted(CoreModule.java:19)
    org.infinispan.factories.GlobalComponentRegistry.modulesManagerStarted(GlobalComponentRegistry.java:326)
    org.infinispan.factories.GlobalComponentRegistry.postStart(GlobalComponentRegistry.java:300)
    org.infinispan.manager.DefaultCacheManager.internalStart(DefaultCacheManager.java:707)
```

## The fix

`../../../../native-builtins/src/infinispan_local.rs`:

1. **`native_dcm_get_cache`**: guarded with `is_real_dcm()` (already existed
   for `start`/`stop`). For a real `DefaultCacheManager`, delegates directly
   to the real `internalGetCache(String)` — what `getCache()`/`getCache(String)`'s
   own real bytecode calls — instead of fabricating a synthetic `Cache`. The
   name argument is forwarded as-is for `getCache(String)`; for no-arg
   `getCache()` it falls back to reading the real `defaultCacheName` field.

2. **The Cache-instance natives** (`put`/`get`/`remove`/`containsKey`/`size`/
   `clear`/`evict`/`addListener`/`removeListener`/`getName`/`putIfAbsent`/
   `replace`, registered on `CLS_CACHE`/`CLS_CACHE_IMPL`/`CLS_CACHE_ADVANCED`)
   needed the identical treatment — confirmed necessary, not optional: once
   `getCache()` hands back real `Cache` objects for a real manager,
   Keycloak's own application-level cache usage (`cache.put`/`get` for
   `realms`/`sessions`/etc.) would otherwise hit these still-synthetic-only
   natives on a REAL object. Each was given an `is_real_cache()` guard that
   delegates to the real underlying implementation via a **different**
   method name/descriptor than the one natively overridden (so the call
   doesn't just re-enter the same native) — found by decompiling the real
   `CacheImpl` class (`javap -c`) for each public method's actual delegate
   target:
   - `put(K,V)` → `put(K,V,Metadata)`, `putIfAbsent(K,V)` →
     `putIfAbsent(K,V,Metadata)`, `replace(K,V)` → `replace(K,V,Metadata)` —
     `Metadata` is the real `defaultMetadata` field, read via
     `get_field_by_name`.
   - `get(Object)` / `containsKey(Object)` → build a real
     `InvocationContext` via `invocationContextFactory.createInvocationContext(false, 1)`,
     then call the package-private `get`/`containsKey(Object, long, InvocationContext)`
     overload.
   - `remove(Object)` → `defaultContextBuilderForWrite()` then
     `remove(Object, long, ContextBuilder)`.
   - `size()` → `size(long)`; `clear()` → `clear(long)`; `evict(K)` →
     `evict(K, long)` — all with a `0` flags/time argument.
   - `addListener(Object)` / `removeListener(Object)` → `addListenerAsync`/
     `removeListenerAsync` (returns a `CompletionStage`) then the static
     `org.infinispan.commons.util.concurrent.CompletionStages.join(CompletionStage)`.
   - `getName()` → direct `get_field_by_name(this, "name")` read (matches
     the real method body, which is just `return name;`).

3. **A latent GC-safety bug found and fixed while implementing (2):**
   `native_cache_get`/`containsKey`/`remove` each make TWO calls where the
   second reuses `this`/`key` (Rust-local `ObjectRef`/`Value`s) captured
   *before* the first, and the first call runs real bytecode that can
   trigger a moving GC. Bare Rust locals held across a re-entrant
   `ctx.invoke_virtual`/`ctx.invoke` call are not automatically kept valid —
   the entry-level pinning `safe_native_call` does for a native's *incoming*
   args does not protect a GC triggered by that native's *own* subsequent
   calls (identical hazard to the one fixed the same day in
   `NativeContextImpl::build_thread_field_holder`, `../../../../vm/src/vm/vm_exec.rs`,
   for `Thread`'s `FieldHolder` construction). Added `pin_this_and_key`/
   `read_this_and_key` helpers (`ctx.pin_native_root`/`read_native_pin`/
   `unpin_native_roots`) and used them in all three functions.

## `is_real_cache()` — three failed attempts before the working one

Getting a reliable real/synthetic discriminator for `Cache` objects took
significantly more iteration than `is_real_dcm()` did for the manager, and
is worth documenting in detail so the next person doesn't repeat the same
dead ends. Verified via a regression probe: `new DefaultCacheManager()` +
`getCache("probe")` + `put`/`get`/`containsKey`/`size`/`getName`/
`putIfAbsent`/`replace`/`remove` — the OLD synthetic path, which must keep
working unchanged (real Infinispan jars are on the classpath, but this
specific no-arg constructor overload is still natively shimmed as
synthetic).

1. **`get_field_by_name(this, "config")`** — reasoned that `config` is only
   ever populated by real `createCache` wiring, never by the synthetic
   branch. Wrong: `CacheImpl`'s real field order is
   `invocationContextFactory`(0), `commandsFactory`(1), `invoker`(2),
   `config`(3) — immediately adjacent to the synthetic 3-slot
   (`CACHE_FIELD_HANDLE`/`NAME`/`MANAGER`) layout override — and reading it
   back for a synthetic (never-really-constructed) cache gave a non-null-
   looking value instead of `Object(None)`, so `is_real_cache` returned
   `true` for a synthetic `Cache`. The regression probe's `put()` then ran
   REAL `CacheImpl.put` bytecode and NPE'd on `this.config.unsafe()`.

2. **`get_field_by_name(this, "componentRegistry")`** — moved to a field
   much further into the real field list, past where the round-trip issue
   was assumed to be confined (mirroring `is_real_dcm`'s own doc comment
   about `globalComponentRegistry` "sitting well past" the affected range
   for `DefaultCacheManager`). Failed identically — same NPE, same probe.
   This falsified the "only the first few indices are affected" model the
   `is_real_dcm` doc comment had assumed; whatever's actually happening is
   broader or different for `CacheImpl` specifically.

3. **`get_field(this, CACHE_FIELD_HANDLE)` matching `Value::Long(bits) if
   bits != 0`** — mirrored `cache_from_field`'s own existing handle check
   (raw slot 0, not by-name). Added temporary `eprintln!` tracing to settle
   it empirically rather than keep guessing, and the trace was decisive:
   *immediately* after `native_dcm_get_cache`'s synthetic branch writes
   `Value::Long(handle)` into that exact slot, reading it back in the very
   next native call gave `Object(None)`, not the `Long`. The write is
   silently lost. Slot 0's REAL declared type is a reference
   (`invocationContextFactory`); storing a `Long` bit pattern into a heap
   slot the GC treats as reference-typed doesn't round-trip — a genuine
   type-mismatched-write bug, not a low-index-only quirk. (Side effect of
   this finding: the pre-existing `cache_from_field`'s "fast path" via
   `CACHE_FIELD_HANDLE` has quietly never actually worked once real
   Infinispan jars are on the classpath — every call was already silently
   falling through to its `CACHE_FIELD_NAME` string-based fallback, which
   is exactly why this was never noticed before now.)

**What actually worked:** the same debug trace confirmed
`get_field_by_name(this, "globalComponentRegistry")` on the *manager*
correctly read back `Object(None)` for the synthetic case — by-name reads
of *some* fields on *some* classes do round-trip correctly; it isn't a
blanket rule about by-name lookups or about index vs. name. Rather than
chase which specific field/class combinations happen to work, switched to a
mechanism with **direct empirical proof**: `cache_from_field`'s existing,
long-working fallback proves that storing and reading back an **object
reference** via a raw slot index is reliable (it has relied on
`CACHE_FIELD_NAME`, slot 1, round-tripping a `String` reference for as long
as the synthetic Cache path has existed). `native_dcm_get_cache`'s synthetic
branch also stores the owning manager into `CACHE_FIELD_MANAGER` (slot 2)
as an object reference — a real cache never does this (slot 2 there is
really `invoker`, an `AsyncInterceptorChain`, never a `DefaultCacheManager`).
So the final, working `is_real_cache`:

```rust
fn is_real_cache(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    match ctx.get_field(this, CACHE_FIELD_MANAGER) {
        Value::Object(Some(mgr)) => {
            let cid = ctx.class_id_of_object(mgr);
            ctx.class_name_of_id(cid).as_deref() != Some(CLS_MANAGER)
        }
        _ => true,
    }
}
```

reads slot 2 back and checks the **runtime class** of whatever object is
there, rather than testing any field's *value* for null-ness — a
reference-identity check sidesteps the round-trip gamble entirely. Verified
via the regression probe (now passes cleanly, `SYNTHETIC_REGRESSION_PROBE_OK`)
and via `RealmModelTest` (the original `cache.config` NPE is gone).

## Verification

- Regression probe (`new DefaultCacheManager()` + `getCache()` +
  `put`/`get`/`containsKey`/`size`/`getName`/`putIfAbsent`/`replace`/`remove`,
  the OLD synthetic path): passes cleanly (`SYNTHETIC_REGRESSION_PROBE_OK`).
- All 21 pre-existing `infinispan_local` Rust unit tests: pass unchanged.
- `RealmModelTest` via `KcRunner` (real Infinispan 16.0.8 classpath): the
  originally reported `cache.config` NPE is gone. `internalStart()`
  completes; `GlobalConfigurationManagerImpl.postStart()` runs cleanly. Under
  `--nojit`, execution proceeds through the ENTIRE cache-manager lifecycle —
  protostream schema registration (~30+ builtin `.proto` files), JGroups
  cluster topology recovery, and cache-manager teardown
  (`EmbeddedCacheManager` reaches `STOPPED`) — with no further Infinispan/
  Cache-native crash. Two **new, distinct, unrelated** residuals surfaced
  one/two layers deeper (both JIT/VM-core, not Infinispan-specific): a
  JIT-adjacent bytecode-decode error reachable only with JIT on, and a now
  fixed STW cross-thread-takeover hang during Netty `EventLoopGroup` shutdown
  reachable only with `--nojit`; see
  [keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md](keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md).

## Prerequisite fix bundled in the same commit

`../../../../native-io/Cargo.toml` had `libc = "0.2"` scoped to
`[target.'cfg(unix)'.dependencies]`, but the `sun/nio/ch/NativeSocketAddress`
native probes added by the prior `JGroupsTransport` fix (`../../../../native-io/src/lib.rs`)
reference `libc::sockaddr_in`/`sockaddr_in6`/`AF_INET`/`AF_INET6`
unconditionally — and the `libc` crate's Windows target module doesn't
define those structs/constants at all (verified against libc 0.2.182's
`src/windows/`; only `src/unix/*` define BSD socket types). This broke
`cargo build --release` for the whole workspace on Windows. Fixed by gating
just that registration block with `#[cfg(unix)]` (Windows never had these
12 natives registered before the prior fix either, so this is a
no-regression restore of buildability, not a functional change) — libc stays
Unix-gated in `../../../../Cargo.toml`, matching the file's other Unix-only
`libc::flock`/`fcntl` usage (already `#[cfg(target_family = "unix")]`-gated
at the function level).
