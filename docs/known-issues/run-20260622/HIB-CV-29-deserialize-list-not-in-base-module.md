# HIB-CV-29 — Java deserialization fails: `StreamCorruptedException: List implementation not in base module`

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** High — breaks `ObjectInputStream` round-trip of common objects; **deterministic, `--nojit`**, HotSpot PASS
**Status:** Confirmed; message is CratonVM-internal

---

## Symptom

`org.hibernate.orm.test.jpa.EntityManagerTest` (`testSerializableException`) serializes
an exception via `ObjectOutputStream` and reads it back via `ObjectInputStream`:

```java
ObjectOutput out = new ObjectOutputStream(stream); out.writeObject(e);
...
ObjectInputStream in = new ObjectInputStream(byteIn); in.readObject();  // <-- throws
```

CratonVM throws:

```
java.io.StreamCorruptedException: List implementation not in base module.
```

HotSpot: PASS.

## Why it's a real CratonVM bug

- Deterministic, reproduces standalone under `--nojit` (not the JIT family).
- HotSpot PASS.
- `"List implementation not in base module"` is **not a JDK message** — it is a
  **CratonVM-internal** error from CratonVM's `ObjectInputStream`/serialization
  implementation. CratonVM appears to gate which `List` implementations it will
  deserialize on "base module" membership and rejects a legitimate one (the object
  graph being deserialized contains a `List` — e.g. an exception's serialized
  fields / suppressed-exceptions / stacktrace-related list).

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args -Dcraton.trace=1 \
  CratonRunner <list-with-org.hibernate.orm.test.jpa.EntityManagerTest> 0
# testSerializableException -> RuntimeException: StreamCorruptedException: List implementation not in base module.
```

A tighter repro: `ObjectOutputStream.writeObject` then `ObjectInputStream.readObject`
on any object holding a standard `java.util.*` `List` (e.g. `Arrays.asList(...)`,
`ArrayList`, an exception carrying a list).

## Suggested next step for a fixer

Grep CratonVM for the literal string `"List implementation not in base module"` —
it points straight at the serialization code path that classifies/admits `List`
implementations during `readObject`. The "base module" check is too strict and
rejects valid platform `List` types.

## Triage

Real, deterministic, independent of the JIT. Serialization is widely used
(caching, clustering, RMI, session replication) — **good hand-off** for whoever
owns serialization. The CratonVM-internal message makes it directly greppable.
