# Bug 25 — `NPE: Cannot invoke cacheSerializedValue on null` (generated-message array null hole)

**Severity:** High — the single largest CratonVM-only FAIL cluster: **~20 failing
tests across 13 classes**, all protocol request/response serialization. HotSpot OK.

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
