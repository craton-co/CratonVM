> **Moved to tracked known-issues:** [docs/known-issues/hibernate-deserialization-sessionfactory-reconnect-null.md](../../../../docs/known-issues/hibernate-deserialization-sessionfactory-reconnect-null.md)

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
[FAIL-throwable-stacktrace-order-reversed.md](FAIL-throwable-stacktrace-order-reversed.md), now fixed, which
should make the precise NPE site visible on a re-run.)

## Next step

Re-run with the stack-order fix to capture the exact NPE frame; then trace
`SessionFactoryRegistry.findSessionFactory(uuid, name)` on CratonVM (registry population at SF build vs the
UUID/name used at deserialization). Likely a registry-keying or UUID bug — possibly localized, but not yet
pinpointed.
