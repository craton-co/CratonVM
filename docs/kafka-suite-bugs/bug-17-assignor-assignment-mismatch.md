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

## Affected classes (partial — append more later)
- consumer.RangeAssignorTest (16/44)
- consumer.CooperativeStickyAssignorTest (rc=1)
- consumer.internals.AbstractStickyAssignor / sticky assignor tests (append from full run)
