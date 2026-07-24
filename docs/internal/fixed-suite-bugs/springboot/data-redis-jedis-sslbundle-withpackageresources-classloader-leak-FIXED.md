# `DataRedisAutoConfigurationJedisTests.testRedisConfigurationWithSslBundle` — `@WithPackageResources` classloader leak — FIXED

**Status: FIXED 2026-07-23**, `native-builtins/src/classloader_real.rs::cl_real_load_class_base`.

## Symptom (as originally filed)

`DataRedisAutoConfigurationJedisTests` (`@ClassPathExclusions("lettuce-core-*.jar")`)
was 22/23 `PASS` on CratonVM. The one failure,
`testRedisConfigurationWithSslBundle` (also annotated
`@WithPackageResources("test.jks")`), failed with:

```
BeanCreationException: Error creating bean with name 'redisConnectionFactory'
  defined in class path resource [.../LettuceConnectionConfiguration.class]:
Failed to instantiate [LettuceConnectionFactory]:
  Factory method 'redisConnectionFactory' threw exception with message:
  io.lettuce.core.SslVerifyMode
Caused by: java.lang.NoClassDefFoundError: io.lettuce.core.SslVerifyMode
```

Confirmed cratonvm-specific (HotSpot passes 23/23) and deterministic/isolated
(reproduces identically running the method alone via `run-single-method.ps1`).

## Root cause

`@WithPackageResources`'s `ResourcesExtension.beforeEach` installs a
`ResourcesClassLoader` — a plain `ClassLoader` subclass with NO overrides of
`loadClass`/`findClass` — as the thread context classloader, parented on
`context.getRequiredTestClass().getClassLoader()`. For this test, that
parent correctly resolves to the exclusion-aware
`ModifiedClassPathClassLoader` (confirmed via `CRATONVM_FORNAME_TRACE` +
ad-hoc tracing: the loader chain `ResourcesClassLoader → 
ModifiedClassPathClassLoader → PlatformClassLoader` was correct, and
`ModifiedClassPathClassLoader`'s `excludedPackages` field was populated).
So the classloader *identity* chain was never the problem — the bug was in
delegation logic.

`ClassLoader.loadClass(String)` for `ResourcesClassLoader` dispatches to the
real-JDK-mode native `cl_real_load_class_base`
(`native-builtins/src/classloader_real.rs`) — **not**
`native-builtins/src/classloader.rs`'s same-named functions, which are dead
code in real-JDK mode (see
[[reference_dual_registration_classloader_vs_classloader_real]]). Step 0 of
`cl_real_load_class_base` correctly recognizes `ModifiedClassPathClassLoader`
as a user-defined parent and invokes its real `loadClass` bytecode directly
(JVMS §5.3 parent-first delegation) — which correctly throws
`ClassNotFoundException` for `io.lettuce.core.RedisClient` (excluded via its
own, deliberately filtered URL list). But that `Err(...)` result didn't
match the `if let Ok(Some(Value::Object(Some(mirror)))) = ... { return ...
}` pattern guarding the early-return, so it fell through *silently* (a bare
`_ => {}`) to "1. Standard VM class loading" a few lines later — a lookup
against CratonVM's flat, loader-blind global class store, which mixes
together every loader's classpath and so still had `RedisClient` (it's on
the process's overall test classpath, just not on
`ModifiedClassPathClassLoader`'s deliberately narrowed one). That global
lookup succeeded, silently un-doing the exclusion the parent's `loadClass`
had just correctly enforced. `@ConditionalOnClass(RedisClient.class)` then
saw `RedisClient` as present, so `LettuceConnectionConfiguration`'s
`redisConnectionFactory()` bean method executed instead of being gated off
by the `@ClassPathExclusions`-driven condition — and it then failed on a
different, unrelated missing symbol (`SslVerifyMode`) reached only from
inside that now-wrongly-active bean method.

Confirmed via targeted `eprintln!` tracing (env-gated, since removed) at
each candidate dispatch layer — this took multiple wrong turns before
landing on the actual live function, since CratonVM has two *entirely
separate* implementations of `ClassLoader.loadClass`'s parent-delegation
logic (`classloader.rs` vs `classloader_real.rs`), and several other
plausible-looking dispatch layers in between (`native_class_for_name`'s
loader-loadClass invoke, `ucl_find_class`'s "permissive findClass"
fallback, `invoke_on_class_shared_inner`'s `check_override` allow-list) that
all turned out to be either dead code in real-JDK mode or simply not on the
call path for this specific receiver chain.

## Fix

In `cl_real_load_class_base`, track whether the user-defined parent's
`loadClass` was actually invoked and did NOT hand back a class
(`parent_user_defined_authoritative_miss`). When true, skip "1. Standard VM
class loading"'s flat-global-store fallback — the parent already ran its
own full, authoritative delegation/exclusion logic; falling through to the
loader-blind global store after an explicit refusal defeats any loader
whose whole purpose is exclusion.

```rust
let mut parent_user_defined_authoritative_miss = false;
if let Some(parent) = parent {
    if crate::classloader::is_user_defined_loader(ctx, parent) {
        match ctx.invoke_virtual(parent, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(class_name_obj))]) {
            Ok(Some(Value::Object(Some(mirror)))) => return Ok(Some(Value::Object(Some(mirror)))),
            _ => { parent_user_defined_authoritative_miss = true; }
        }
    }
}
if !parent_user_defined_authoritative_miss && !defer_to_find_class && !scoped_user_chain {
    // ... global store lookup ...
}
```

## Verification

- `testRedisConfigurationWithSslBundle` alone: FAIL → PASS.
- Full `DataRedisAutoConfigurationJedisTests` class: 23/23 PASS (no
  regression from the other 22, which already passed before this fix and
  don't go through this exact code path — they don't use
  `@WithPackageResources`, so `ResourcesClassLoader` is never installed).
- Full non-docker `module/spring-boot-data-redis` (13 classes, 130 tests):
  12/13 PASS immediately; the 13th
  (`DataRedisAutoConfigurationTests`) hit a 300s timeout under
  `-Parallel 2` but PASSes cleanly (56/56, ~288s) run in isolation with a
  longer timeout — a pre-existing throughput/timeout artifact (large test
  count, shared-host contention), not a regression from this fix.

## Affected classes

| Module | Class | Method |
|---|---|---|
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationJedisTests` | `testRedisConfigurationWithSslBundle` |

Likely fixes the same shape of bug for any other `@WithPackageResources`
(or otherwise `ResourcesClassLoader`-wrapped) test whose classloader parent
chain includes a user-defined loader with its own real, exclusion-aware
`loadClass` override — not confirmed against other classes this session.
