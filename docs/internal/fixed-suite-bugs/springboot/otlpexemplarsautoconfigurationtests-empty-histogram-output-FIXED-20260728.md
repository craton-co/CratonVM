# `OtlpExemplarsAutoConfigurationTests`: OTLP histogram exemplar output

**Status: FIXED 2026-07-28**

## Root cause

This was not a Brave trace-context problem. CratonVM's native
`ArrayList.removeAll(Collection)` bridge snapshots the removal collection with
`collect_collection_elements`. For a real-JDK
`Collections.singletonList(null)`, that helper considered the null `element`
field to mean that the singleton had no element at all. Consequently,
`removeAll(Collections.singletonList(null))` retained null entries instead of
removing them.

Micrometer's OTLP exemplar collector builds an `ArrayList` from its exemplar
array and removes null entries in exactly that way. The leaked null then made
protobuf's `HistogramDataPoint.Builder.addAllExemplars` abort publication with
`Element at index 0 is null`, leaving the test's captured OTLP output empty.

## Fix

`native-collections/src/lib.rs` now identifies
`Collections$SingletonList` and `Collections$SingletonSet` by receiver class
and returns their `element` field even when it is Java null. This preserves the
JDK collection contract for null singleton elements without changing the
non-null snapshot path.

## Validation

- Targeted real-JDK probe: `new ArrayList<>(Arrays.asList(null, "kept", null))`
  followed by `removeAll(Collections.singletonList(null))` now returns true and
  leaves exactly `[kept]` under CratonVM `--nojit`.
- `OtlpExemplarsAutoConfigurationTests`: **6/6 PASS** with JIT (32.822s).
- `OtlpExemplarsAutoConfigurationTests`: **6/6 PASS** with `--nojit` (32.460s).
- Matching HotSpot fixture baseline: **6/6 PASS**.

The requested `apps/spring-boot` checkout had a missing
`core/spring-boot` runtime artifact during preflight, so the final CratonVM
matrix used the intact matching Spring Boot fixture at
`C:\craton\CratonVM-spring-boot-rerun-20260717\apps\spring-boot` read-only.
