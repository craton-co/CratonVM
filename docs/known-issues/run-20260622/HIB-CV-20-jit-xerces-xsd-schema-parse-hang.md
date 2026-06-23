# HIB-CV-20 — JIT hang parsing XSD schemas via Xerces `SchemaFactory.newSchema()`

**Run:** full Hibernate ORM suite, 2026-06-22
**Binary:** `C:/craton/CratonVM-hibtest/target/release/cvhibtest.exe` (dev `c863b23e`, branch `chore/hibernate-fullsuite-20260622`)
**Severity:** High — true hang (not slowness); JIT-specific; default config
**Status:** Root-caused to JIT, minimal Hibernate-free repro attached

---

## Symptom

`org.hibernate.boot.jaxb.internal.stax.LocalXmlResourceResolverTest` **hangs** (no result within the 300 s harness cap → recorded `HANG`).

| | HotSpot (baseline) | CratonVM (dev, default) |
|---|---|---|
| status | **PASS** | **HANG** |
| tests | 23 | hangs in test #1 |
| wall | **4.69 s** | **> 300 s** (killed) |

A 4.7 s HotSpot run cannot be explained by linear slowdown — this is a genuine hang.

Per-method probe (`HangProbe`) shows it hangs inside the very first parameterized
invocation, which is the first access to `org.hibernate.boot.xsd.MappingXsdSupport`:

```
@@START [1] resolve_namespace_localResource("http://java.sun.com/xml/ns/persistence/orm", "org/hibernate/jpa/orm_1_0.xsd")
   ... never finishes ...
```

---

## Root cause (localized)

`LocalXmlResourceResolver.resolveEntity` (line 39) touches `MappingXsdSupport`,
whose `<clinit>` eagerly builds **13 `XsdDescriptor`s**, each calling
`LocalXsdResolver.resolveLocalXsdSchema()`:

`hibernate-core/.../boot/xsd/LocalXsdResolver.java:83`
```java
return SchemaFactory.newInstance( W3C_XML_SCHEMA_NS_URI )
        .newSchema( new StreamSource( url.openStream() ) );   // Xerces XSD parse
```

So first access parses 13 JPA/Hibernate ORM XSDs (≈ 58 KB – 165 KB each) through
`com.sun.org.apache.xerces.internal.jaxp.validation.XMLSchemaFactory`.

### Elimination ladder (each step a standalone probe vs HotSpot)

| Hypothesis | Test | Result |
|---|---|---|
| `String.matches()` / SBR-02 native regex | 121 URI×URI `matches()` combos (`MinRegex`) | all pass instantly → **not regex** |
| reading the XSD jar/file resource stream | read `orm_1_0.xsd` fully both ways (`MinStream`) | 58971 bytes OK → **not stream I/O** |
| a single bad XSD | parse each of the 13 XSDs alone (`MinSchema`) | every one parses OK → **not one schema** |
| **multiple parses in one process** | parse all 13 in declaration order (`MinSeq`) | **HANG at the 5th–6th parse** |
| heap pressure / GC | same with `--Xmx 6g` | still hangs at 6th → **not heap size** |
| background-compile worker / OSR race | same with `CRATONVM_BG_COMPILE=0` | still hangs → **not bg-worker; foreground JIT** |
| **JIT** | same with `--nojit` | **completes all 13, rc=0** ✅ |

### Conclusion

The hang is **JIT-specific and in the foreground compile path**. With `--nojit`
the identical 13-parse sequence finishes cleanly; with JIT on (the default) it
reproducibly hangs at about the 6th schema parse. Disabling the background
compile worker (`CRATONVM_BG_COMPILE=0`) does **not** help — so this is a
**foreground JIT miscompile producing an infinite loop** (not a bg-compile/OSR
installation race) in a hot Xerces XSD-parsing routine that goes hot around the
6th schema. This matches the historical `HIB-CV-02 jit-xerces-skipstring-hang`.

Full config matrix in `matrix.txt`.

---

## Minimal repro (no Hibernate, no JUnit)

`MinSeq.java` (attached) parses the 13 ORM XSDs from the hibernate-core resources
in `MappingXsdSupport` declaration order, one fresh `SchemaFactory` each.

```
# HANGS (default / JIT on):
cvhibtest.exe --java-home <jdk25> @common.args MinSeq        # rc=124, parsed 5-6/13
# COMPLETES (JIT off):
cvhibtest.exe --java-home <jdk25> --nojit @common.args MinSeq # rc=0, parsed 13/13, @@ALLDONE
# HotSpot: instant, all 13.
```

(`@common.args` only supplies the classpath that contains the `.xsd` resources;
they live in `hibernate-core/target/resources/main/org/hibernate/...`.)

---

## Impact

- Directly causes the `LocalXmlResourceResolverTest` HANG.
- `MappingXsdSupport` is on the Hibernate XML-mapping / `persistence.xml` /
  `orm.xml` bootstrap path, so this JIT hang is a strong candidate root cause for
  an as-yet-unknown share of the suite's other `HANG` results (any test whose
  SessionFactory bootstrap touches `MappingXsdSupport` under JIT). Tests that
  bootstrap purely from annotations and never load `MappingXsdSupport` are spared
  — which is why some SessionFactory tests pass while others hang.

## Suggested next step for a fixer

Run the `MinSeq` repro under `CRATONVM_DBG_JITC` / `CRATONVM_DBG_JIT_DISASM` to
identify the Xerces method compiled around the 5th–6th parse and inspect the
emitted loop (likely a `char`/`String` scan — "skipString"-class routine). If
it's a background-compile/OSR race, `CRATONVM_BG_COMPILE=0` will side-step it.

## Repro files
- `MinSeq.java` — 13-XSD sequential parse (primary repro)
- `MinSchema.java` — single-XSD parse (control: passes)
- `HangProbe.java` — per-test JUnit progress probe used to localize the test method
