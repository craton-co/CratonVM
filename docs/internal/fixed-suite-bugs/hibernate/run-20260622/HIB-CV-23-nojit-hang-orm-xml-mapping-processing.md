# HIB-CV-23 — Non-JIT hang processing `orm.xml` mapping fragments (Ejb3Xml* family)

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** High — true hang, **reproduces under `--nojit`** (independent of the JIT bug family)
**Status:** Confirmed real hang; localized to the orm.xml mapping-processing path; exact root cause open

---

## Symptom

The `org.hibernate.orm.test.annotations.xml.ejb3.Ejb3Xml*` tests hang. Under
`--nojit` (so this is **not** the HIB-CV-20/21 JIT hang) the class hangs on its
**first** test method and never produces a result:

```
@@START testMultipleJoinColumns() [.../Ejb3XmlOneToOneTest/...]
   ... never finishes (killed at 180s) ...
```

| Class | HotSpot | CratonVM `--nojit` |
|---|---|---|
| `Ejb3XmlOneToOneTest` | PASS 11/11, 7.2 s | **HANG** on test #1 (>180 s) |
| `Ejb3XmlManyToOneTest` | PASS 9/9, 6.7 s | **HANG** |
| `Ejb3XmlElementCollectionTest` | PASS 28/28, 8.7 s | **HANG** |

A 7 s / 11-test class hanging on the first test for >180 s is a hang, not linear
slowness (~270× is not a slowdown).

## Localization

`Ejb3Xml*` extend `Ejb3XmlTestCase`, whose helper
`getAttributeMember(entity, field, "<name>.orm1.xml")` reads a small `orm.xml`
mapping fragment and runs it through Hibernate's XML mapping pipeline:

`org.hibernate.boot.models.xml.spi.XmlPreProcessor` / `XmlProcessor` /
`PersistenceUnitMetadataImpl` (JAXB binding + XSD validation of the mapping XML).

So the hang is in **reading/binding/validating an `orm.xml` mapping fragment** in
the interpreter. It is distinct from HIB-CV-20 (that was Xerces `SchemaFactory.newSchema()`
under JIT; here JIT is off and the pure XSD parse completes — see HIB-CV-20's
`MinSeq` which finishes under `--nojit`). This is the JAXB/StAX **mapping-document**
path, not the schema-compile path.

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args \
  HangProbe org.hibernate.orm.test.annotations.xml.ejb3.Ejb3XmlOneToOneTest
# -> @@START testMultipleJoinColumns(); never @@END; rc=124
```

## Open / next step for a fixer

Root cause not yet isolated to a single routine. Next: reproduce with a tiny
program that runs `XmlPreProcessor.preProcessXmlResources(...)` /
`XmlProcessor.processXml(...)` on one `*.orm1.xml` fragment and capture a native
stack (e.g. `CRATONVM_DBG_*` / a thread dump) to find the spin location — likely
a StAX `XMLEventReader` loop or a JAXB binder loop that never advances on
CratonVM's StAX implementation.

## Triage

Real, non-JIT, reproducible hang affecting the `Ejb3Xml*` family. Independent of
the dominant JIT bug — a separate fix. Good **hand-off** candidate for whoever
owns StAX/JAXB XML handling.
