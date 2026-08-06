# `mock()` of any `ThreadLocal` subclass empties every `ThreadLocal` in the process

**Status: FIXED 2026-08-06.** Found by the ByteBuddy-woven-class sweep run while
closing
[`infinispan-configurationbuilder-retransform-verify-FIXED.md`](infinispan-configurationbuilder-retransform-verify-FIXED.md),
and filed separately because it is a different defect.

## Symptom

`Mockito.mock(org.springframework.core.NamedThreadLocal.class)` fails with

```
java.lang.NullPointerException: Cannot invoke "java.lang.Boolean.booleanValue()"
  because the return value of "java.lang.ThreadLocal.get()" is null
	at org.mockito...InlineDelegateByteBuddyMockMaker.lambda$new$2(...:292)
	at org.mockito...MockMethodAdvice.isConstructorMock(MockMethodAdvice.java:224)
	at org.mockito...inject.MockMethodDispatcher.isConstructorMock(...:57)
	at java.lang.ThreadLocal.<init>(ThreadLocal.java)
	at org.mockito...access.MockMethodInterceptor.<init>(MockMethodInterceptor.java:42)
```

…and then **every subsequent `mock()` in the same JVM fails identically** — 89
of 728 attempted mocks in the sweep that found it. HotSpot 25 on the same
classpath mocks all of them cleanly.

The Mockito frames are the messenger, not the message: this is a VM defect that
happens to take out Mockito's own internals first.

## Root cause

CratonVM serves `get` / `set` / `remove` / `initialValue` / `withInitial` /
`<init>` on `java.lang.ThreadLocal` and `java.lang.InheritableThreadLocal` from
registered natives (`NativeKind::Intrinsic`, `native-builtins/src/
phases_early.rs`) whose store is **a Rust thread-local keyed by the
ThreadLocal's identity hash** — `TL_MAP` and `tl_with_initial_suppliers` — not
`Thread.threadLocals`. The real JDK bodies are loaded and concrete; the natives
shadow them.

Mockito's inline mock maker instruments the target's **whole superclass chain**,
so mocking any `ThreadLocal` subclass retransforms `java.lang.ThreadLocal`
itself. Confirmed with `CRATONVM_DBG=retransform`:

```
[RETRANSFORM] retransformClasses0 called with 3 classes
[RETRANSFORM]   [0] java/lang/Object                              original_bytes=2412
[RETRANSFORM]   [1] java/lang/ThreadLocal                         original_bytes=8110
[RETRANSFORM]   [2] org/springframework/core/NamedThreadLocal     original_bytes=1480
```

(8110 bytes is byte-for-byte the real `java.base` `ThreadLocal.class`.)

A redefinition deliberately **suppresses native shadows** so an agent's woven
bytecode can run — `native_shadow_suppressed_by_redefine`, which is right for an
ordinary class. `java.lang.ThreadLocal` was not on the immunity list, so from
that moment `ThreadLocal.get()` ran the real JDK body, which looks in a
`ThreadLocalMap` the natives never populated and finds nothing. Every value the
process had ever `set` became invisible, and a `ThreadLocal.withInitial(…)`
handle — which on this VM is a plain synthetic `java/lang/ThreadLocal` with the
supplier held in a Rust side table, not a real `SuppliedThreadLocal` — fell
through to `ThreadLocal.initialValue()`, whose native answers `null`
unconditionally. Mockito holds `ThreadLocal.withInitial(() -> false)` fields, so
the very next `booleanValue()` NPE'd, inside mock creation.

## Measurement

One binary, one variable — the mock target — via
`probes/ThreadLocalRetransformProbe.java` and `--dump-native-registry`:

| | mock a `ThreadLocal` subclass | mock any other class |
|---|---:|---:|
| `ThreadLocal.get` native invocations | **4** | 23 |
| value `set` before the mock | **null** | `"hello"` |
| `withInitial(() -> TRUE).get()` | **null** | `true` |
| `new ThreadLocal<>()` | **NPE** | ok |
| probe verdict | **PROBE-FAIL 4** | PROBE-OK |

Two rows stayed green throughout and are worth keeping in mind: a `set`+`get`
pair *both* issued after the retransform round-trips (both ran the real body and
agreed), and an `initialValue()` override on a subclass answers correctly (that
override is the subclass's own bytecode, resolved before this gate).

## Fix

`redefine_immune_thread_local_native`, added to **both** aggregators —
`redefine_immune_forced_native` (slow path) and `redefine_immune_layout_native`
(invoke-cache paths). Adding it to one only is the mistake the synthetic
collection entry already made on 2026-07-31, where the probe went from 32 broken
operations to 18 instead of to 0 because the cache sites re-assemble their own
chain; `thread_local_immunity_reaches_the_invoke_cache_sites_too` asserts the
layout aggregator directly rather than trusting both were edited.

This is the fifth member of the same family — reflection metadata,
StringBuilder, `Path.toString`, JFR metadata, `FileHandler`, StampedLock, BC
crypto math and the synthetic collections all carry the same immunity for the
same reason: **CratonVM keeps their state somewhere the real JDK body cannot
see, so ceding the method to woven bytecode produces silent nonsense rather than
an interceptable mock.**

### Accepted limitation

`when(mock.get()).thenReturn(x)` on a mocked `ThreadLocal` will not intercept —
`get()` resolves to `java/lang/ThreadLocal.get`, which is now immune and answers
from the side table. That is the same trade the collections arm already makes,
and the same reasoning applies: one un-stubbed mock costs far less than silent
data loss on every ThreadLocal in the process. A `ThreadLocal` *subclass*'s own
methods are unaffected and remain mockable
(`ordinary_classes_stay_evictable` pins that).

## Verdict

| arm | before | after |
|---|---|---|
| `ThreadLocalRetransformProbe`, ThreadLocal-subclass target | PROBE-FAIL 4 | **PROBE-OK (10/10)** |
| `ThreadLocalRetransformProbe`, ordinary target (control) | PROBE-OK | **PROBE-OK** |
| `MockManyProbe`, 4-class list with `NamedThreadLocal` first | 0 ok / 4 fail | **4 ok / 0 fail** |
| HotSpot 25, both probes | PROBE-OK | PROBE-OK |
