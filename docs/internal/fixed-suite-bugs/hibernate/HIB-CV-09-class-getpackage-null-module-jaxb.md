# HIB-CV-09 — `Class.getPackage()` returns a `Package` with a null `module` → `Package.getDeclaredAnnotations()` NPEs → every `orm.xml` / mapping-document parse fails

**Severity:** High — fails every Hibernate test that loads a JPA `orm.xml` / XML mapping document. ≥8 classes in the first ~340 census classes.
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `native-builtins/src/lang_class.rs`)
**Mode:** Interpreter (JIT-off census) — not a JIT bug.
**HotSpot:** not affected.

## Symptom

```
org.hibernate.boot.InvalidMappingException: Could not parse mapping document:
  org/.../orm.xml (RESOURCE)
Caused by: java.lang.NullPointerException: Cannot invoke getClassLoader on null
   …org.glassfish.jaxb.runtime.v2.model.annotation.RuntimeInlineAnnotationReader.getPackageAnnotation
   …org.glassfish.jaxb.runtime.v2.runtime.JAXBContextImpl.<init>
   …org.hibernate.boot.jaxb.internal.MappingBinder.mappingJaxbContext
```

Affected classes (census, growing): `DuplicateTest`, `IdTest`, `HibernateSequenceTest`,
`OrmXmlEnumTypeTest`, `FetchProfileTest`, `OrmXmlIndexTest`, `CompositeKeyDeleteTest`,
`EmbeddableWithOneToMany_HHH_11302_xml_Test`, …

## Root cause

Hibernate's `MappingBinder` builds a JAXB context over its mapping model classes
(`org.hibernate.boot.jaxb.mapping.spi.*`). Glassfish JAXB reads each model class's **package**
annotations (`@XmlSchema`) via `getPackageAnnotation` → `Package.getAnnotation` →
`Package.getDeclaredAnnotations()` → `packageInfo()` → `module().getClassLoader()`.

CratonVM's `Class.getPackage()` (`native_class_get_package`) synthesises a `java.lang.Package`
and sets its `name` (and manifest fields), but **never sets the `module` field**. So the real
JDK `Package.getDeclaredAnnotations()` bytecode calls `module().getClassLoader()` on a **null**
module → `NullPointerException: Cannot invoke getClassLoader on null`, aborting the whole parse.

Probe (`PkgProbe` on `JaxbEntityMappingsImpl`):

| | HotSpot | CratonVM (before) |
|--|---------|-------------------|
| `class.getModule()` | unnamed module | unnamed module (non-null) |
| `class.getPackage()` | `package org.hibernate.boot.jaxb.mapping.spi` | `Object@…` (synthetic, null module) |
| `package.getDeclaredAnnotations()` | `[]` | **NPE: Cannot invoke getClassLoader on null** |

`Class.getModule()` already returns a valid module — only the **Package's** module link was missing.

## Fix

In `native_class_get_package`, after building the synthetic `Package`, set its `module` field from
the class's own module (obtained via `invoke_virtual(this, "getModule", …)`). The real
`packageInfo()` then resolves the (usually absent) `<pkg>.package-info` class to a sentinel and
`getDeclaredAnnotations()` returns `[]`, exactly like HotSpot.

## Repro

`PkgProbe` (in `.cratonvm-suite/`): `JaxbEntityMappingsImpl.class.getPackage().getDeclaredAnnotations()`.
Before: NPE. After: `[]` (matches HotSpot).
