# HIB-CV-23 — `XMLInputFactory.createXMLEventReader(Source)` unimplemented → `AbstractMethodError` parsing `persistence.xml`

**Severity:** Medium — **6 CV-only failing classes**. JAXB `persistence.xml` parsing via StAX from a `StreamSource`.
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `native-builtins/src/xml_stax.rs`).
**Mode:** Interpreter (JIT-off census).
**HotSpot:** not affected.

## Symptom

6 classes fail (HotSpot passes all):

```
java.lang.AbstractMethodError: method javax/xml/stream/XMLInputFactory
  .createXMLEventReader(Ljavax/xml/transform/Source;)Ljavax/xml/stream/XMLEventReader;
  has no Code attribute
```

Classes: `jpa.persistenceunit.PersistenceXmlParserTest`, `ExcludeUnlistedClassesTest`, `DuplicatePersistenceUnitNameTest`, `jpa.boot.DefaultToOneFetchTypeTests`, `jpa.jakarta.JakartaXmlSmokeTests`, `jpa.compliance.PersistenceUnitNameTests`.

## Root cause

`javax.xml.stream.XMLInputFactory` is abstract; CratonVM provides a *synthetic* factory instance (`xml_stax.rs`) with natives for the entry points it implements: `createXMLStreamReader(InputStream|Reader)` and `createXMLEventReader(InputStream|Reader)`. The `createXMLEventReader(javax.xml.transform.Source)` overload was **not** registered.

Hibernate's `PersistenceXmlParser` (`hibernate-core .../jpa/boot/spi/PersistenceXmlParser.java:321`) binds JAXB from `new StreamSource(inputStream)`, and the JAXB unmarshaller calls `createXMLEventReader(Source)`. With no native for that descriptor, the call dispatched to the **abstract** `XMLInputFactory.createXMLEventReader(Source)` method (no Code attribute) → `AbstractMethodError`.

## Fix

Register `createXMLEventReader(Ljavax/xml/transform/Source;)Ljavax/xml/stream/XMLEventReader;`. The native resolves a `StreamSource`'s payload — `getInputStream()`, else `getReader()`, else open `getSystemId()` as a URL — then builds the same synthetic cursor `XMLStreamReader` and wraps it in the real JDK `com.sun.xml.internal.stream.XMLEventReaderImpl`, identical to the existing `InputStream`/`Reader` overloads.

## Verification

All 6 classes now green vs HotSpot: `PersistenceXmlParserTest` 4/4, `ExcludeUnlistedClassesTest` 1/1, `DefaultToOneFetchTypeTests` 7/7, `JakartaXmlSmokeTests` 2/2, `PersistenceUnitNameTests` 2/2, `DuplicatePersistenceUnitNameTest` 1/1 — **0** `AbstractMethodError`.
