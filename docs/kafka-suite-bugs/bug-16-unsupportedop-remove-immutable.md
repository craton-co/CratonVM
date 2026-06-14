# Bug 16 — `UnsupportedOperationException: remove` (wrong collection mutability)

**Severity:** Low/Medium — 3 failures; `consumer.internals.NetworkClientDelegateTest`.
Reproduces under `--nojit`. HotSpot clean.

## Symptom
```
=> java.lang.UnsupportedOperationException: remove
```
Code calls `iterator.remove()` / `collection.remove(...)` on a collection that is
**immutable on CratonVM but mutable on HotSpot** — i.e. a CratonVM intrinsic
returned an unmodifiable view (or a fixed-size `Arrays.asList`) where the JDK
returns a mutable collection.

## Root cause (to pin down)
A CratonVM collection intrinsic returns the wrong (unmodifiable) collection type.
Candidates: `new ArrayList<>(...)` / `Collectors.toList()` / `Map.values()` /
`stream().collect(...)` returning an immutable result, or an `Arrays.asList`-backed
list reaching a `.remove()`. Pin to the exact call site in the failing method and
compare the returned collection's mutability vs HotSpot.

## ✅ FIXED (2026-06-13) — ArrayDeque iterator.remove()

Pinned with `apps/kafka/tests/repro/MutProbe.java`, which exercises `iterator
.remove()` across every candidate collection. Only ONE diverged:
`new ArrayDeque<>(coll)` → CratonVM `iterator.remove UNSUPPORTED` vs HotSpot OK.
(`new ArrayList/LinkedList/HashSet(coll)`, `Collectors.toList/toSet`,
`Map.values/keySet`, `stream.collect` were all already mutable.)

ROOT CAUSE: CratonVM's `ArrayDeque$Itr` is snapshot-backed (field 0 = array,
field 1 = cursor) and registered only `hasNext`/`next` — no `remove`, and no
backing-deque reference. So `remove()` fell to the `java/util/Iterator` default,
which throws `UnsupportedOperationException`. Kafka `NetworkClientDelegate`
cleans up unsent requests via `iterator.remove()` over an `ArrayDeque` — hit
exactly that.

FIX (`native-collections/src/lib.rs`): `native_ad_iterator` now stores the
backing `ArrayDeque` in iterator field 2; a new `native_ad_itr_remove`
(registered for `java/util/ArrayDeque$Itr`) removes the last-returned element
from the live deque via the existing `removeFirstOccurrence` logic (snapshot
left intact for continued iteration). Verified: `MutProbe` ArrayDeque now `OK`
(all 11 cases match HotSpot, no regression); `NetworkClientDelegateTest` no
longer throws `UnsupportedOperationException` (it now runs, 2/6 pass).

RESIDUAL (separate bug, NOT bug-16): the remaining 4 `NetworkClientDelegateTest`
failures are now `ConfigException: Missing required configuration
"key.deserializer"` — a consumer-config-validation divergence unrelated to
collection mutability (HotSpot supplies the default). Track separately.

## Affected classes (partial — append more later)
- consumer.internals.NetworkClientDelegateTest
