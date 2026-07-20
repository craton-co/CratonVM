# `ThreadPoolExecutor` stat-method natives: `invoke_virtual_bytecode_only` self-recursion on an overriding subclass

**Status: FIXED 2026-07-20**

## Symptom

`ReactiveOAuth2ResourceServerAutoConfigurationTests`
(`module/spring-boot-security-oauth2-resource-server`) started failing 5/50
tests with `StackOverflowError` immediately after merging dev commit
`57a57fa1f` ("fix(scheduling): real ThreadPoolExecutor stat methods returned
synthetic-slot garbage in real-JDK mode") — discovered while verifying an
unrelated fix
([`spring-boot-restclient-residuals-FIXED.md`](spring-boot-restclient-residuals-FIXED.md))
against fresh `dev`. The class had passed 50/50 immediately before that
merge.

```
JUnit Jupiter:ReactiveOAuth2ResourceServerAutoConfigurationTests:customTypeValidatorCanReplaceDefaultWhenUsingIssuerUri()
    => java.lang.StackOverflowError
       reactor.core.scheduler.BoundedElasticScheduler$BoundedScheduledExecutorService.isShutdown(BoundedElasticScheduler.java:1001)
       reactor.core.scheduler.BoundedElasticScheduler$BoundedScheduledExecutorService.isShutdown(BoundedElasticScheduler.java:1001)
       ... (repeats to the stack limit)
```

Reproduced identically across all 5 of that class's `isShutdown()`-touching
tests.

## Root cause

`57a57fa1f` had already found and fixed exactly this bug shape for
`ThreadPoolExecutor.shutdownNow()`: `ScheduledThreadPoolExecutor` overrides
`shutdownNow()` as `return super.shutdownNow();`, so calling the CratonVM
native for `shutdownNow` via `invoke_virtual_bytecode_only(this, ...)`
(dynamic/virtual dispatch on the receiver's *concrete* class) resolved
straight back to that same overriding `shutdownNow()`, whose
`invokespecial ThreadPoolExecutor.shutdownNow` re-entered the very same
native — infinite recursion. The commit fixed `shutdownNow` with
`invoke_special_bytecode_only("java/util/concurrent/ThreadPoolExecutor", ...)`
(static dispatch on the named class, matching real `invokespecial`
semantics) but left every *other* real-executor stat method it touched or
introduced in the same change — `getPoolSize`, `getActiveCount`,
`getCorePoolSize`, `getMaximumPoolSize`, `isShutdown`, `isTerminated`,
`awaitTermination`, `getTaskCount`, `getCompletedTaskCount` — on
`invoke_virtual_bytecode_only`, carrying the identical hazard.

Reactor's `BoundedElasticScheduler$BoundedScheduledExecutorService` (a
`ScheduledThreadPoolExecutor` subclass Reactor uses internally for its
bounded-elastic scheduler) overrides `isShutdown()` for its own
pool-recycling bookkeeping and calls `super.isShutdown()`. That super call
hit the CratonVM `isShutdown` native, which (post-`57a57fa1f`,
pre-this-fix) used `invoke_virtual_bytecode_only(this, "isShutdown", ...)`
— virtual dispatch on `this`'s concrete class re-resolved back to
`BoundedScheduledExecutorService.isShutdown()` itself, looping forever.
`spring-security-oauth2-resource-server`'s reactive JWT decoder
configuration builds one of these schedulers internally, which is why the
failure surfaced there specifically.

## Fix

`native-collections/src/lib.rs`: every one of the 9 stat-method natives
above now uses `invoke_special_bytecode_only("java/util/concurrent/
ThreadPoolExecutor", <method>, <descriptor>, &[receiver, ...args])` instead
of `invoke_virtual_bytecode_only`, mirroring the pattern `57a57fa1f` already
established for `shutdownNow`. Static dispatch on the named class and
virtual dispatch on the receiver's concrete class produce identical results
whenever nothing overrides the method in between (the case
`57a57fa1f`'s own `ConcurrentTaskExecutorTests`/`ThreadPoolTaskExecutorTests`/
`ThreadPoolTaskSchedulerTests`/`DecoratedThreadPoolTaskExecutorTests`
coverage exercises), so this is behavior-preserving for every receiver
without an overriding subclass, and only changes (fixes) the previously
broken case.

One mechanical follow-up: the register-time closures for `getPoolSize`,
`getActiveCount`, `awaitTermination`, `getTaskCount`, and
`getCompletedTaskCount` are passed to `NativeMethodRegistry::register` as
plain `fn` pointers; referencing the outer `let tp = "java/util/concurrent/
ThreadPoolExecutor";` binding from inside the closure body turns it into a
capturing closure, which no longer coerces to `fn`. Each closure spells the
class name out as a literal instead of reusing `tp`.

Verified:
- `cargo test --release -p cratonvm-native-collections thread_pool`: pass
  (`thread_pool_executor_stats_registered`).
- `cargo test --release -p cratonvm-vm --test threadpoolexecutor_prestart_regression --test wave1_c_executor`: pass (6/6).
- `ReactiveOAuth2ResourceServerAutoConfigurationTests`: back to 50/50 PASS
  (JIT on).
