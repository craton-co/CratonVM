# Bug 17 — consumer partition-assignor produces wrong assignments

**Severity:** Medium — `RangeAssignorTest` 16/44, plus `CooperativeStickyAssignorTest`
(rc=1) and sticky/abstract assignor assertion failures. Reproduces under `--nojit`.
HotSpot clean.

## Symptom
Assignment-result assertions fail, e.g.:
```
=> org.opentest4j.AssertionFailedError:
   expected: <{consumer1=[t1-0, t1-1], consumer2=[t1-2, ...]}>
   but was:  <{consumer1=[t1-0, ...], consumer2=[...]}>
=> AssertionFailedError: Failed to find expected partition AAAAAAAAAAAAAAAAAAAAAA:foo-0
```
The computed `Map<String, List<TopicPartition>>` assignment differs from HotSpot —
either different partition→consumer distribution or different **ordering** of the
partition lists.

## Root cause (to pin down)
The assignor logic (range/sticky/cooperative-sticky) depends on deterministic
ordering of maps/sets (e.g. `TreeMap`/`LinkedHashMap`/sorted topic-partition
iteration). A CratonVM `Map`/`Set` ordering or sort intrinsic divergence yields a
different-but-"valid-looking" assignment, failing exact-match assertions. The
`Failed to find expected partition AAAA…:foo-0` (`AAAA…` = all-zero `Uuid`) variant
may also be entangled with [bug-18](bug-18-fetcher-topicid-zeroed.md) (topicId
zeroing).

Pin down by comparing, for one fixed input, the assignor's intermediate sorted
topic/partition iteration order vs HotSpot.

## Narrowing (2026-06-13, current dev) — it's the RACK-AWARE path, ruled out the obvious causes

`RangeAssignorTest` now 37/44 (was 16/44; the gap closed as other dev fixes
landed). **The 7 remaining failures are exactly the 7 rack-aware /
co-partitioning tests** — `testRackAwareAssignmentWith{UniformSubscription,
NonEqualSubscription,UniformPartitions,UniformPartitionsNonEqualSubscription,
CoPartitioning}`, `testCoPartitionedAssignmentWithSameSubscription`,
`testRackAwareStaticMemberRangeAssignmentPersistentAfterMemberIdChanges`. All
non-rack `RackConfig`-parameterized tests pass == HotSpot.

The failing assignments are CONTIGUOUS where HotSpot is rack-strided — i.e.
CratonVM behaves as if rack-awareness is off for these. But the obvious causes
are all RULED OUT (verified == HotSpot via standalone probes in
`apps/kafka/tests/repro/`):
- basic `RangeAssignor.assign` (no racks) — `RangeProbe.java` ✓
- `Collections.disjoint(Set,Set)` (the `useRackAwareAssignment` short-circuit) —
  `DisjointProbe.java` ✓ (the old wrong-intrinsic is already fixed)
- `AbstractPartitionAssignor.useRackAwareAssignment(...)` in isolation —
  `RackAwareProbe.java` ✓ (true for differing partition racks, false for
  all-on-all)
- a simple rack-aware `assignPartitions` (1 topic, 6 partitions, replicas on
  differing racks, 3 consumers each in a distinct rack) — `RackAssignProbe.java`
  ✓ (`0,2 / 1,3 / 4,5` == HotSpot)

So the divergence needs one of the SPECIFIC failing configs — multiple
co-partitioned topics (equal partition counts), non-equal subscriptions, or
static members — exercising `RangeAssignor.assignWithRackMatching` /
`assignCoPartitionedWithRackMatching` (the latter keys off a
`LinkedHashMap<String,Optional<String>>`) or the `TopicAssignmentState`
constructor's input computation (`consumers` LinkedHashMap, `partitionRacks`
HashMap, `unassignedPartitions` HashSet). Next step: replicate
`testRackAwareAssignmentWithCoPartitioning`'s exact setup (decompile the test
method for the rack/partition layout) and diff the intermediate
`TopicAssignmentState` against HotSpot to find the diverging collection/stream.

## Affected classes (partial — append more later)
- consumer.RangeAssignorTest (16/44)
- consumer.CooperativeStickyAssignorTest (rc=1)
- consumer.internals.AbstractStickyAssignor / sticky assignor tests (append from full run)
