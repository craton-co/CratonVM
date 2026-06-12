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

## Affected classes (partial — append more later)
- consumer.internals.NetworkClientDelegateTest
