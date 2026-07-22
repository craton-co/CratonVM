# Bug 12 — `NullPointerException: Cannot invoke apiKey on null` (NodeApiVersions / ApiKeys iteration)

## FIXED (2026-06-12) — `ImplicitLinkedHashCollection.toArray()` returned null holes

Not an `ApiKeys.values()` / `EnumSet` / `Collectors` bug — those iterate cleanly
(verified: `ApiKeys.apisForListener(...)` yields 0 nulls via its iterator). The
real NPE is inside `NodeApiVersions.<init>` (NodeApiVersions.java:102), which
iterates `new LinkedList<>(response.data().apiKeys())` — a copy of an
`ApiVersionsResponseData$ApiVersionCollection` (extends kafka's
`ImplicitLinkedHashCollection`, an open-addressing hash array `elements` with
NULL holes for empty buckets).

`new LinkedList<>(coll)` / `new ArrayList<>(coll)` call `coll.toArray()`, which
resolves to `AbstractCollection.toArray()` — intercepted by CratonVM's
`native_al_to_array` → `collect_collection_elements`. That helper fell through to
the generic ArrayList heuristic (`elements[0..size]`), which surfaced the hash
holes: `toArray()` returned **5 nulls** of 62 (HotSpot: 0). The copies then
carried nulls and `apiVersion.apiKey()` NPE'd.

**Fix** (`native-collections/src/lib.rs`): added a dedicated
`ImplicitLinkedHashCollection` branch (detected via `is_subclass`) to
`collect_collection_elements` that reads `elements` and drops the null holes
(mirroring the real `iterator()`, which walks the embedded linked list). Also
guarded the `ArrayList.<init>(Collection)` native's ArrayList-layout fast path to
skip ILHC sources so they take the same corrected path.

**Verified:** `NodeApiVersionsTest` → **14/14** (was 11/14), matching HotSpot.
Repro: `apps/kafka/tests/repro/ApiProbe3.java` (`toArray()` 5 nulls → 0 nulls).

---


**Severity:** Medium — 6 failures; `NodeApiVersionsTest` 11/14. Reproduces under
`--nojit` and JIT. HotSpot clean.

## Symptom
```
NodeApiVersionsTest.testUsableVersionLatestVersions(ListenerType) ...
=> java.lang.NullPointerException: Cannot invoke apiKey on null
```
Failing methods iterate over `ApiKeys` / `ApiMessageType.ListenerType` and call
`.apiKey()` on an element that is `null` on CratonVM but non-null on HotSpot.

## Root cause (to pin down)
An iteration source yields a `null` element on CratonVM — likely one of:
- `ApiKeys.values()` / an `EnumSet`/`EnumMap` over `ApiKeys` returning a null slot, or
- `ApiMessageType.values()` (generated message type enum) with a null entry, or
- a stream/`Collectors` intrinsic producing a list with a null hole.

Reproduce by iterating `ApiKeys.values()` / `ApiMessageType.values()` (or the
`apisForListener(ListenerType)` path) under CratonVM and finding the null element.
Likely an enum-`values()` / generated-enum intrinsic or a `Collectors` bug (cf.
the documented "wrong intrinsic stubs" family).

## Affected classes (partial — append more later)
- clients.NodeApiVersionsTest (3 failures: testUsableVersionLatestVersions ZK_BROKER/BROKER/CONTROLLER)
