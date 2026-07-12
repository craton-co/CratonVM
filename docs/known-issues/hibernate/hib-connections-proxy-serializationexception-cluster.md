# Hibernate connection-provider / proxy tests — `SerializationException: could not deserialize`

| | |
|---|---|
| **Status** | 🔴 OPEN — 5 classes, HotSpot confirmation pending. |
| **Discovered** | 2026-07-11 full 4548-class suite audit, `dev` post `44f16ee2`+. |
| **Area** | Java serialization round-trip, connection-provider and lazy-proxy test infrastructure. |

## Symptom

```
org.hibernate.type.SerializationException: could not deserialize
```
Appears multiple times per affected class (each hit is a distinct test
method), alongside otherwise-unrelated single-occurrence assertion failures
in the same classes (e.g. `AggressiveReleaseTest` also fails
`[connection not maintained through flush]` once) — the serialization
failure looks like the dominant, shared symptom; the other single failures
per class may be downstream consequences or unrelated.

## Affected classes (5)

```
connections.AggressiveReleaseTest
connections.BasicConnectionProviderTest
connections.CurrentSessionConnectionTest
connections.SuppliedConnectionTest
proxy.ProxyTest
```

Four of five are in the `connections.*` package (Hibernate's suite of tests
that swap different `ConnectionProvider` implementations and check session/
connection lifecycle behavior under each) — these commonly serialize a
session or a value through a round-trip as part of exercising connection
release/reacquire behavior. `proxy.ProxyTest` also touches Hibernate's lazy-
proxy serialization path (proxies must be `Serializable` so detached
entities can round-trip).

## Root-cause hypothesis (not yet confirmed)

`SerializationException: could not deserialize` is Hibernate's wrapper
around a `java.io.ObjectInputStream` failure. Common CratonVM-side causes
for this shape elsewhere in this codebase have included: `serialVersionUID`
computation mismatches for a class (see the 2026-07-05 finding for
`CacheKeyEmbeddedIdEnanchedTest`, a similar "InvalidClassException:
serialVersionUID mismatch" wrapped the same way), or a native/synthetic
class whose real-JDK field layout doesn't line up with what
`ObjectStreamClass` expects to read back. Given this hits both plain
connection-provider round-trips and proxy serialization, the affected value
type may be shared (e.g. `Serializable` session identifiers, or the proxy's
own serialized form) — worth checking what's actually being
serialized/deserialized in a representative failing test first.

## Next steps (not yet done)

- Get the wrapped/cause exception (Hibernate's `SerializationException`
  wraps the real `IOException`/`ClassNotFoundException`/
  `InvalidClassException` — the harness's truncated capture doesn't show
  it).
- Identify what object type is actually being serialized in one of these
  tests (likely visible from the test source/entity involved) and check its
  CratonVM-computed `serialVersionUID` against HotSpot's, per the established
  pattern for this bug family.
- Confirm CratonVM-specificity with a HotSpot run (pending).
