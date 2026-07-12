# Hibernate boot-time model building — QName ClassCastException + Jandex indexing exceptions (co-occurring cluster)

| | |
|---|---|
| **Status** | OPEN — QName portion fixed on current `dev`; Jandex indexing remains reproducible. |
| **Discovered** | 2026-07-11 full 4548-class suite audit, `dev` post `44f16ee2`+. |
| **Area** | Hibernate's `hibernate-models` boot-time metadata layer — Jandex-based annotation indexing and XML-mapping processing (`org.hibernate.boot.models.*`). |

## Symptom

Two distinct exception shapes recur together in the same small set of
`boot.models.*`/scanning classes:

**QName ClassCastException** (4 classes):
```
java.lang.ClassCastException: javax.xml.namespace.QName cannot be cast to javax.xml.namespace.QName
```
The class being cast to itself by name is the classic loader-identity
symptom (two distinct `QName` `Class` objects — likely loaded by different
classloaders/namespaces — being treated as the same type).

**Jandex indexing exception** (5 classes):
```
org.hibernate.models.jandex.internal.JandexIndexerHelper$JandexIndexingException: Error indexing standard types
org.hibernate.HibernateException: Error indexing class for Jandex index - jakarta.persistence.Access
```

`org.hibernate.orm.test.boot.models.xml.XmlProcessingSmokeTests` originally
hit both symptoms.  The QName exceptions and the Jandex exception were later
shown to be separate defects (see progress below).

## Affected classes (9, union of both symptoms)

```
boot.models.xml.XmlProcessingSmokeTests        (both: QName CCE x2, Jandex indexing x1)
boot.models.xml.dynamic.DynamicModelTests      (QName CCE)
annotations.xml.ejb3.Ejb3XmlManyToOneTest      (QName CCE)
intg.AdditionalMappingContributorBasicColumnTests  (QName CCE)
jpa.boot.discovery.SimpleTests                 (Jandex indexing)
boot.models.SourceModelTestHelperSmokeTests    (Jandex indexing)
boot.models.xml.complete.CompleteXmlInheritanceTests  (Jandex indexing)
boot.models.annotation.SimpleAnnotationUsageTests     (Jandex indexing)
bootstrap.scanning.ScannerTest                 (Jandex indexing; also has the separately-tracked jar-scanning `orm.xml` gap)
```

## Progress (2026-07-12)

### QName ClassCastException: fixed on `dev`

`origin/dev` commit `da3f6a7b1` includes the interface-stamped synthetic
receiver/native-registry dispatch fix.  Re-running
`XmlProcessingSmokeTests` from that baseline no longer produced a QName
`ClassCastException`; the prior loader-identity symptom is resolved.

The focused XML run instead reached Jandex processing.  This establishes that
the QName and Jandex reports in the initial cluster were co-occurring, not one
shared failure.

### Jandex indexing: still open, narrowed to parser cursor alignment

The remaining focused failure is deterministic under `--nojit` while indexing
`java/util/Comparator.class` from Hibernate's baseline Java types:

```
org.jboss.jandex.Indexer.processCode(Indexer.java:762)
org.jboss.jandex.Indexer.skipFully(Indexer.java:323)
java.io.EOFException
```

The resource stream itself is valid: direct `ByteArrayInputStream.skip(1024)`
and `DataInputStream.skipBytes(1024)` both return 1024, and a standalone
long-local probe reproduces Jandex's `(high << 16) | low` calculation
correctly.  The bad value is therefore not a byte-array `skip` contract or a
category-2-local/long-shift error.

Frame and stream traces show that Jandex consumes the first valid `Code`
attribute (a five-byte code body) and then reaches `processCode` with an
impossible `codeLength` of 87488 for a 9052-byte class.  It subsequently
exhausts the resource in `skipFully`.  Passing an already-buffered stream and
enabling JIT produce the same failure.  The current boundary is the cursor or
attribute classification between the first valid `Code` attribute and the next
`processCode` invocation, most likely in Jandex's `processMethodInfo` /
`processAttributes` path or the VM execution of that path.

An uncommitted, separate-worktree investigation was started at
`C:\craton\CratonVM-hib-models-xml-jandex-20260712-002` on branch
`codex/hib-models-xml-jandex-20260712-002`.  It contains experimental stream
bridge changes only; none are validated or merged.  Keep that worktree for
diagnosis, but do not treat the experiments as a fix.

## Root-cause hypothesis (updated)

The QName CCE was a loader-identity/native-dispatch issue and is no longer a
candidate root cause for the Jandex exception.  The Jandex failure now points
to a VM execution or field/cursor-alignment defect in its class-file parser
path, not corrupted class-resource bytes.

## Next steps (not yet done)

- Retain the QName regression outcome when the Jandex fix is eventually made;
  do not reopen the already-resolved loader-identity theory.
- Instrument the Jandex `processMethodInfo` / `processAttributes` cursor and
  `constantPoolAnnoAttrributes` classification around the first
  `Comparator.class` `Code` attributes.  Compare each attribute index, length,
  and cursor with HotSpot before changing stream behavior again.
- Once the parser cursor is correct, rerun all nine classes from the affected
  list with JIT disabled and enabled, then move this note to `docs/internal`.
