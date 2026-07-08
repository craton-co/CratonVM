# LinkedCaseInsensitiveMap this$0 deserialization NPE (FIXED 2026-07-08)

| | |
|---|---|
| Status | FIXED, verified 2026-07-08 on Azure `dev` with a fresh `release-with-debug` binary. |
| Area | Java serialization, `HashMap.readObject`, `LinkedHashMap.removeEldestEntry`. |
| Repro tests | `org.springframework.util.MimeTypeTests.serialize()` and `org.springframework.http.MediaTypeTests.serialize()`. |

## Symptom

The Spring serialization tests failed during deserialization with:

```text
java.lang.NullPointerException: Cannot invoke "org.springframework.util.LinkedCaseInsensitiveMap.removeEldestEntry(java.util.Map$Entry)" because "this.this$0" is null
```

Spring's `LinkedCaseInsensitiveMap` stores entries in an anonymous non-static
`LinkedHashMap` subclass. That subclass overrides `removeEldestEntry` and
captures the enclosing `LinkedCaseInsensitiveMap` through the compiler-generated
`this$0` field.

## Root Cause

The field restoration itself was not the failing operation. The native
`HashMap.readObject` replay path inserted serialized entries through the normal
map `put` helper. For `LinkedHashMap` subclasses, normal live puts call the
overridable `removeEldestEntry` hook after a new node is linked.

During Java deserialization, however, the subclass object graph is still being
rebuilt while the superclass `HashMap.readObject` body runs. Invoking Spring's
override at that point reads the not-yet-restored synthetic outer reference and
throws the `this$0` NPE. HotSpot avoids this by using `putVal(..., evict=false)`
from `HashMap.readObject`, suppressing the eviction hook while entries are
replayed.

## Resolution

Current `dev` mirrors HotSpot: `native_hashmap_read_object` calls
`native_map_put_evict(..., false)`, which routes to `native_lhm_put_evict` for
`LinkedHashMap` receivers and skips `removeEldestEntry` only for the
deserialization replay. Ordinary live puts still pass `evict=true`, so bounded
