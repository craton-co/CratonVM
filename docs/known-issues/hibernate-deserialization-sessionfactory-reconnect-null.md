# HIB-DEV-05 — deserialized `EntityManager`/`SessionFactory` has null SessionFactory → NPE (5 classes)

**Severity:** Medium — fails Hibernate's serialization round-trip tests.
**Status:** 🔴 OPEN — root narrowed; deeper than a single native (SessionFactory reconnection). Handoff/investigate.
**Mode:** Interpreter (JIT-off).
**HotSpot (JDK 25):** all 5 **PASS**.

## Symptom

`NullPointerException: Cannot invoke getMappingMetamodel on null` / `Cannot invoke getClassLoaderService on null`, after a serialize→deserialize round-trip:

```java
EntityManager em = scope.getEntityManagerFactory().createEntityManager();
out.writeObject(em);
em = (EntityManager) in.readObject();   // deserialized em's SessionFactory is null
// subsequent use -> SF.getMappingMetamodel()/getClassLoaderService() NPE
```

**Affected (CV-only):** `jpa.ejb3configuration.EntityManagerSerializationTest`,
`jpa.ejb3configuration.EntityManagerFactorySerializationTest`,
`jpa.serialization.EntityManagerDeserializationTest`,
`connections.SuppliedConnectionTest`, `connections.BasicConnectionProviderTest`.

## Root cause (narrowed)

The generic Java deserialization machinery works on CratonVM — a standalone `Serializable` with a private
`readObject(ObjectInputStream)` **and** `readResolve()` is correctly invoked (repro `jsonrepro/SerRepro.java`,
matches HotSpot). So the failure is specific to **Hibernate's SessionFactory reconnection**: a serialized
`SessionImpl`/`SessionFactoryImpl` stores the factory UUID + name and, on deserialization, reconnects to the
live factory via `SessionFactoryRegistry.INSTANCE.findSessionFactory(uuid, name)`. On CratonVM that lookup
yields **null**, so the deserialized object's `factory` field stays null → NPE on first use.

Candidate causes (needs the SessionFactoryRegistry path traced): the registry's UUID/name keying or its
backing map lookup not matching on CratonVM, or the SF not being registered under the expected key. (Note:
diagnosis was impeded by the reversed-stack-trace bug — see
[FAIL-throwable-stacktrace-order-reversed.md](../../apps/hibernate-orm/cratonvm-bug-reports/dev-run-20260617/FAIL-throwable-stacktrace-order-reversed.md), now fixed, which
should make the precise NPE site visible on a re-run.)

## Refinement 2026-06-18 (#2) — PINNED to a serialized-stream STRUCTURE divergence in `SessionImpl` (deep core-serialization bug)

With the throwable-order fix in, the deser NPE now has a **readable stack**:
```
NullPointerException: Cannot invoke "SessionFactoryImplementor.getMappingMetamodel()" because the receiver is null
  … ObjectInputStream.readObject
  at org/hibernate/internal/SessionImpl.readObject(SessionImpl.java:2843)
  at org/hibernate/engine/internal/StatefulPersistenceContext.deserialize(StatefulPersistenceContext.java:1942)
```
`StatefulPersistenceContext.deserialize` (called from `SessionImpl.readObject`) calls `session.getFactory()
.getMappingMetamodel()` and **`session.getFactory()` is null** — the `factory` field (set by
`AbstractSharedSessionContract.readObject` → `SessionFactoryImpl.deserialize(ois)` →
`locateSessionFactoryOnDeserialization(uuid,name)`) never got reconnected.

**Root pinned via a minimal probe** (`jsonrepro/EmUuidProbe.java`, real `Configuration.buildSessionFactory()`
+ `createEntityManager()` + serialize): the SF uuid **is** present in CratonVM's serialized `EntityManager`
stream — but **at a completely different offset**:

| | serialized `EntityManager` stream | factory-uuid offset |
|---|---|---|
| HotSpot | 3197 bytes | **1244** (early, super's region) |
| CratonVM | 3087 bytes | **2863** (near the end) |

So CratonVM's `ObjectOutputStream` writes the SessionImpl object graph in a **different structure** (the
factory lands ~1600 bytes later, total 110 bytes shorter). On read, `SessionFactoryImpl.deserialize` consumes
the stream at the position the *bytecode* expects (early) but CratonVM put the factory data elsewhere → it
reads the wrong bytes → `locate` misses → `factory = null` → NPE.

**Ruled out as the cause** (all match HotSpot on CratonVM): class-hierarchy `writeObject`/`readObject`
ordering at **2 levels** (`jsonrepro/HierSer.java`) **and 3 levels** (`jsonrepro/Hier3.java`) — super-first,
correct offsets; `ObjectInputStream.writeUTF`/`readUTF` round-trip (`jsonrepro/UtfProbe.java`); the
`writeReplace`/`readResolve`/`findSessionFactory` reconnect of a SF serialized **directly**
(`jsonrepro/SfRegProbe.java`, `deser SF == original`). So it is **not** any of those — it is a
**field/handle/back-reference ORDER divergence** in CratonVM's `ObjectOutputStream` for a complex,
deep object graph (the difference only manifests on the real SessionImpl, not on 2–3 level toy hierarchies).

**Conclusion:** confirmed, precisely located — but a **deep core-serialization bug** in CratonVM's
`ObjectOutputStream`/`ObjectStreamClass` object-graph encoding, not a localized native fix. Next step is
serialization-stream forensics: dump and diff the TC_OBJECT/TC_CLASSDESC/TC_REFERENCE structure of the CV vs
HotSpot `EntityManager` stream to find the field/handle whose position diverges (the ~1600-byte shift before
the factory). Repros in `.cratonvm-suite/jsonrepro/`.

---

## Refinement 2026-06-18 — generic serialization mechanisms all verified working (gap is Hibernate-specific)

Attempted to localize; **ruled out** every generic cause (all match HotSpot on CratonVM):
- `readObject(ObjectInputStream)` + `readResolve()` invoked correctly (`jsonrepro/SerRepro.java`).
- **`writeReplace()` → proxy → `readResolve()` → registry-lookup reconnect** round-trips correctly and returns
  the **same** live object (`jsonrepro/ReplaceRepro.java`, `back==original`). This is exactly the shape of
  Hibernate's `SessionFactoryImpl` serialization (writeReplace → `SerializableSessionFactory(uuid,name)` →
  readResolve → `SessionFactoryRegistry.findSessionFactory`).
- `ConcurrentHashMap<String,_>` lookup by an **equal-but-distinct** String key (the deserialized UUID) returns
  the value (`jsonrepro/ChmKey.java`) — so `findSessionFactory(uuid)` = `sessionFactoryMap.get(uuid)` is sound.
- **No CV native/stub** intercepts `SessionFactoryRegistry` / `addSessionFactory` / `findSessionFactory` — it
  runs real Hibernate bytecode.

`findSessionFactory(uuid,name)` decompiles to `sessionFactoryMap.get(uuid)` (then a `nameUuidXref`/name
fallback). Since the map + key-equality + reconnect mechanics are all correct, the residual must be
**Hibernate-specific**: either the SF was never `addSessionFactory(uuid,…)`-registered during CratonVM
bootstrap, or the UUID in the serialized form ≠ the registered UUID (or a name/`nameUuidXref` step). Pinpointing
needs a full EMF-bootstrap reproduction that inspects `SessionFactoryRegistry.INSTANCE` state (the JUnit
`EntityManagerFactoryScope` makes a standalone probe non-trivial). **Not a quick localized native fix** —
remains a handoff.

## Next step

Re-run with the stack-order fix to capture the exact NPE frame; then trace
`SessionFactoryRegistry.findSessionFactory(uuid, name)` on CratonVM (registry population at SF build vs the
UUID/name used at deserialization). Likely a registry-keying or UUID bug — possibly localized, but not yet
pinpointed.
