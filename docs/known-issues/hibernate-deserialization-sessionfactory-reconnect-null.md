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
