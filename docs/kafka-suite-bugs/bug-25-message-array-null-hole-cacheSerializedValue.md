# Bug 25 — `NPE: Cannot invoke cacheSerializedValue on null` (null serialization cache)

**Severity:** High — the single largest CratonVM-only FAIL cluster: **~20 failing
tests across 13 classes**, all protocol request/response serialization. HotSpot OK.

> **ROOT-CAUSE CORRECTION (2026-06-13):** the receiver of `cacheSerializedValue` is
> **`org.apache.kafka.common.protocol.ObjectSerializationCache`**
> (`cacheSerializedValue(Object,[B)V`), NOT a message-array element. So the null is the
> **serialization cache itself** being null when a generated message's
> `size(cache,ver)`→`addSize(size,cache,ver)` runs (it caches each String/Bytes field's
> serialized bytes via `cache.cacheSerializedValue(field, bytes)`). The earlier
> "array null hole" theory below is superseded. The cache is created right before, e.g.
> `ObjectSerializationCache cache = new ObjectSerializationCache(); message.size(cache,…)`,
> so the bug is either (a) `new ObjectSerializationCache()` yielding null / a broken
> instance, or (b) the `cache` local/param being lost between creation and the
> `addSize` use. Reproduces under `--nojit` (so NOT the bug-21/22 JIT register-root
> issue). Next: reproduce a single `CreateAclsRequestTest.shouldRoundTripV0`, dump the
> `size()`/`addSize()` `cache` arg, and check `new ObjectSerializationCache()`.
>
> **UPDATE — not reproducible standalone (2026-06-13):** a standalone `CacheProbe`
> matching the test — `new ObjectSerializationCache()`, direct `Data.size/write`, full
> serialize→parse→re-size round trip, AND `new CreateAclsRequest.Builder(d).build(v)` →
> `request.serialize()` with multiple creations — **all succeed** on the current binary
> (cache non-null, no null array elements, correct byte sizes). So the core serialization
> path is sound; the null cache only manifests **inside the JUnit harness**
> (`shouldRoundTripV0/V1` still fail there). Most likely a **GC-timing / instrumentation
> interaction** under the harness's heavier allocation load (Mockito self-attach, JUnit
> reflection) that loses the `cache` local mid-serialization — NOT a deterministic
> serialization bug. Next: instrument the interpreter "invoke … on null" throw site to
> dump the caller method+pc when the method is `cacheSerializedValue` (frame-capped
> `DBG_ATHROW` only reaches the JUnit rethrow), run the real test, and capture the GC
> event around the throw. Remaining open item for the bug-25 cluster.
>
> **UPDATE — pinned via `CRATONVM_DBG_NPE_STACK=1` (existing gate, 30 frames):** the
> null-cache call chain is
> ```
> [51] CreateAclsRequestData$AclCreation.addSize(MessageSizeAccumulator, ObjectSerializationCache, S) pc=86  // -> cache.cacheSerializedValue(field, bytes)
> [50] CreateAclsRequestData.addSize(MessageSizeAccumulator, ObjectSerializationCache, S) pc=75
> [49] org/apache/kafka/common/protocol/Message.size(ObjectSerializationCache, S)I pc=17
> [48] org/junit/platform/commons/util/ReflectionUtils.invokeMethod(Method, Object, Object[]) pc=45
> ```
> Two hard facts: (1) the receiver is a **genuine null** — the interpreter's
> stale/all-zero-header salvage (interpreter.rs ~10440) did NOT fire, so this is
> `Value::Object(None)`, not a GC-collected stale pointer; (2) the test method's own
> frames (`shouldRoundTripV0` → `AbstractRequest.serialize` → `RequestUtils.serialize`)
> are **ABSENT** between JUnit's reflective `invokeMethod` [48] and `Message.size` [49]
> — `size` is reached *directly* from the reflective invoke with a **null cache arg**.
> So the bug is in CratonVM's **reflective invocation path** (`Method.invoke` /
> `ReflectionUtils.invokeMethod` argument marshalling, or a Method-resolution/frame
> anomaly), NOT in message serialization (which is byte-correct standalone). This is why
> it is JUnit-harness-only. **Next:** dump the `Object[] args` (and the resolved target
> Method) inside CratonVM's `Method.invoke` native for the failing call to see where the
> `ObjectSerializationCache` argument becomes null / the wrong method is dispatched.
>
> **UPDATE 2 — genuine null confirmed; degraded-Method ruled out:**
> - Code-path analysis of the throw site (interpreter.rs ~10868) shows it is the `_`
>   arm where the receiver is `Value::Object(None)` — a **genuine null**, definitively
>   NOT a GC-collected stale all-zero-header object (that path salvages at ~10440).
> - `CRATONVM_DBG_MINVOKE=1` + `CRATONVM_DBG_METHOD_INVOKE_BOX=1` on the failing test
>   produced **no degraded-Method hits** → the reflective dispatch resolves the right
>   `Method`; it is not a clazz-null Method bug.
> - The absent `serialize()`/test-method frames between `invokeMethod` and `Message.size`
>   indicate those calls run through CratonVM's **stackless cached-dispatch**
>   (`execute_invokevirtual_cached`/`execute_invokestatic_cached`), which does not push a
>   full scannable frame. **Leading hypothesis:** the `ObjectSerializationCache` local
>   created in `RequestUtils.serialize` is lost (read back as null) because the
>   stackless-dispatched serialize path's locals aren't GC-rooted/preserved under the
>   harness's allocation pressure — the interpreter-side analogue of bug-21/22, but on
>   the stackless path. **Next:** instrument `execute_invokevirtual_cached` (or disable
>   stackless dispatch for the serialize path via the existing bisect gate) and re-run;
>   if the cache survives with stackless dispatch off, the fix is to root/spill locals
>   across the cached-dispatch boundary. This is the remaining bug-25 work; the cluster
>   is otherwise byte-correct (proven by standalone probes).

## Symptom
```
=> java.lang.NullPointerException: Cannot invoke cacheSerializedValue on null
=> java.lang.RuntimeException: Failed to deserialize request {acks=-1,timeout=123,
   partitionSizes=[topic1-1=72]} with type class org.apache.kafka.common.requests.ProduceRequest
```
A Kafka generated-message struct (the elements of a message array field, e.g.
`ProduceRequestData.TopicProduceData[]`, ACL filters, etc.) is **null** when the
serializer walks the collection and calls `cacheSerializedValue()` on each element.

## Affected classes (13)
`RequestResponseTest`, `SimpleExampleMessageTest`, `NullableStructMessageTest`,
`CreateAclsRequestTest`, `DeleteAclsRequestTest`, `DeleteAclsResponseTest`,
`DescribeAclsRequestTest`, `DescribeAclsResponseTest`, `DeleteTopicsRequestTest`,
`OffsetCommitResponseTest`, `StopReplicaRequestTest`, `TxnOffsetCommitResponseTest`,
`UpdateFeaturesRequestTest`. (Related `NPE: Cannot invoke tags/topics/errorCode/value
/setArraySizeInBytes on null` — 10+ more failures — are almost certainly the same
null-hole defect on different generated fields.)

## Root cause (to pin down)
A CratonVM collection/array intrinsic introduces a **null hole** into a generated
message's array/list field during the deserialize→re-serialize round trip. This is
the same family as the already-fixed **bug-12** (`ImplicitLinkedHashCollection.toArray()`
null holes) and **bug-18** (`Uuid` zeroed) — a native collection/`toArray`/clone path
that returns a slot the JDK would have filled. Candidates:
- `ArrayList`/`Arrays.asList`/`toArray(T[])` producing a trailing or interior null
  (cf. bug-14 `removeAll` over-removal),
- the generated `ArrayOf`/`CompactArrayOf` protocol reader writing fewer elements than
  `size`, leaving nulls,
- an `ImplicitLinkedHashCollection`-backed field (ACLs use these heavily).

## Reproduce
```
cd apps/kafka/tests
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 ./kv2.exe -cp ".;$(cat cp.txt)" KRun \
  org.apache.kafka.common.requests.RequestResponseTest
```
Narrow to `testSerialization` / a single `ProduceRequest` round-trip; dump the array
field right after `read(...)` and find which index is null and which intrinsic filled
the backing collection. Fixing the null-hole source should clear all 13 classes at once.

## Note
Verify against the **current** binary (`9d8bba97`) first — the dev merge `b5c709c7`
touched collection intrinsics (CSLM/TreeMap/LinkedHashSet); some of this cluster may
already be reduced. Counts above are from the pre-merge `cv-full2` run.
