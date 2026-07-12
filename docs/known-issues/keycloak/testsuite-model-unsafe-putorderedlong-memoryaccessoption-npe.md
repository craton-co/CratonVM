# testsuite/model: `sun.misc.Unsafe.putOrderedLong()` NPEs on null `MemoryAccessOption`, breaking Netty's MPSC queue construction (blocks all 37 previously-ProcessHandle-crashed classes)

Status: open — genuine CratonVM bug in `sun.misc.Unsafe`'s ordered-write methods, now the dominant blocker for
`testsuite/model` after the `ProcessHandle`/SmallRye linkage bug was fixed

Date observed: 2026-07-11 (refresh rerun against non-passed-before classes, branch fix/keycloak-nonpassed-rerun-v2-20260710,
binary built from dev post-merge which fixed the earlier ProcessHandle/SmallRye crash)

## Summary

All 37 `testsuite/model` classes that previously CRASHed with the `io.smallrye.common.os.Process`/`ProcessHandle`
linkage bug (see `testsuite-model-smallrye-process-processhandle-linkageerror-FIXED.md`, now fixed) get
substantially further this run — but every one of them now hits a **new, different** blocker:

```
=> java.lang.ExceptionInInitializerError
 Caused by: org.infinispan.commons.CacheConfigurationException: Unable to construct a GlobalComponentRegistry!
 Caused by: org.infinispan.commons.CacheException: Unable to construct a GlobalComponentRegistry!
 Caused by: java.lang.RuntimeException: Failed to construct component org.infinispan.executors.non-blocking, path org.infinispan.executors.non-blocking
 Caused by: java.lang.RuntimeException: Failed to construct component io.netty.channel.EventLoopGroup, path io.netty.channel.EventLoopGroup
 Caused by: java.lang.IllegalStateException: failed to create a child event loop
 Caused by: java.lang.NullPointerException: Cannot invoke "sun.misc.Unsafe$MemoryAccessOption.ordinal()"
   sun.misc.Unsafe.beforeMemoryAccessSlow(Unsafe.java:1805)
   sun.misc.Unsafe.beforeMemoryAccess(Unsafe.java:1787)
   sun.misc.Unsafe.putOrderedLong(Unsafe.java:1493)
   io.netty.util.internal.shaded.org.jctools.queues.BaseMpscLinkedArrayQueueColdProducerFields.soProducerLimit(BaseMpscLinkedArrayQueue.java:163)
   io.netty.util.internal.shaded.org.jctools.queues.BaseMpscLinkedArrayQueue.<init>(BaseMpscLinkedArrayQueue.java:202)
   io.netty.util.internal.shaded.org.jctools.queues.MpscUnboundedArrayQueue.<init>(MpscUnboundedArrayQueue.java:44)
   io.netty.util.internal.PlatformDependent$Mpsc.newMpscQueue(PlatformDependent.java:1067)
   io.netty.util.internal.PlatformDependent.newMpscQueue(PlatformDependent.java:1078)
   io.netty.channel.nio.NioEventLoop.newTaskQueue0(NioEventLoop.java:283)
   io.netty.channel.nio.NioEventLoop.newTaskQueue(NioEventLoop.java:154)
   io.netty.channel.nio.NioEventLoop.<init>(NioEventLoop.java:142)
   io.netty.channel.nio.NioEventLoopGroup.newChild(NioEventLoopGroup.java:183)
   io.netty.channel.nio.NioEventLoopGroup.newChild(NioEventLoopGroup.java:38)
   io.netty.util.concurrent.MultithreadEventExecutorGroup.<init>(MultithreadEventExecutorGroup.java:84)
```

## Root cause

`sun.misc.Unsafe.putOrderedLong(Object, long, long)` — a legacy "ordered/lazy" memory-write method (a relaxed
memory-ordering variant of `putLong`, historically used for high-performance lock-free structures before
`VarHandle`s existed) — internally calls `beforeMemoryAccess()` → `beforeMemoryAccessSlow()`, which needs to look
up a `sun.misc.Unsafe$MemoryAccessOption` enum value to classify/validate the access. Under CratonVM, this lookup
returns `null` instead of a valid enum constant, and the subsequent `.ordinal()` call NPEs.

This is triggered here by Netty's vendored/shaded JCTools library (`BaseMpscLinkedArrayQueueColdProducerFields`,
part of Netty's high-performance MPSC — multi-producer single-consumer — lock-free queue implementation used for
its `NioEventLoop` task queues) calling `putOrderedLong()` during queue construction. Since virtually every
Netty `NioEventLoopGroup`/`EventLoopGroup` construction goes through this exact code path to build its event
loops' task queues, **any code that constructs a Netty event loop group under CratonVM is likely to hit this**,
not just Infinispan/Keycloak — Netty is an extremely widely-used networking library.

## Impact

Currently blocks all 37 previously-crashed `testsuite/model` classes from reaching their actual test logic (they
all need an embedded Infinispan cache manager, which needs a Netty `EventLoopGroup`). This is now the dominant
remaining blocker for this module, having been unmasked by the recent `ProcessHandle`/SmallRye linkage fix. Given
Netty's ubiquity, likely affects other modules/suites too wherever a Netty event loop group gets constructed.

## Next steps

1. Search `native-builtins/src/` for CratonVM's `sun.misc.Unsafe` implementation — find
   `beforeMemoryAccess`/`beforeMemoryAccessSlow`/`putOrderedLong` (or wherever `MemoryAccessOption` is
   referenced) and fix the null lookup. Given `putLong`/`putInt` (the non-ordered variants) presumably work fine
   elsewhere in the suite, this is likely specific to the "ordered" family of Unsafe methods
   (`putOrdered{Int,Long,Object}`) and however they classify themselves via `MemoryAccessOption`.
2. Write a minimal standalone repro: call `sun.misc.Unsafe.putOrderedLong(obj, offset, value)` directly (via
   reflection to get an `Unsafe` instance, as is standard) under CratonVM and confirm the NPE reproduces outside
   of any Keycloak/Netty/Infinispan context.
3. Once fixed, re-run all 37 previously-affected `testsuite/model` classes — this may unmask yet another blocker
   (e.g. the previously-documented Liquibase `Scope` corruption bug, `testsuite-model-liquibase-scope-corruption.md`,
   which these classes may not have reached yet either) — check whether that bug is still present or already
   fixed too before concluding the module is healthy.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-unsafe-putorderedlong -ClassList <(printf 'module\tclass\ntestsuite/model\torg.keycloak.testsuite.model.user.UserModelTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh-20260711.exe -JdkHome $jdk
```

## Evidence

37 classes across
`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-before-refresh-shard{1,2,3,4}\all-jit\logs\testsuite_model.*.out.log`,
2026-07-11 refresh rerun with a binary built from `dev` post-merge (which had just fixed the ProcessHandle/SmallRye
linkage bug documented separately).
