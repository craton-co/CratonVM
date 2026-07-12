# Hibernate boot-time model building — QName ClassCastException + Jandex indexing exceptions (co-occurring cluster)

| | |
|---|---|
| **Status** | 🔴 OPEN — 9 distinct classes (some hit both symptoms), HotSpot confirmation pending. |
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

`org.hibernate.orm.test.boot.models.xml.XmlProcessingSmokeTests` hits BOTH
symptoms across its 5 test methods (`found=5 ok=2 failed=3`), suggesting a
shared root cause in the XML/annotation model-building bootstrap rather than
two unrelated bugs.

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

## Root-cause hypothesis (not yet confirmed)

Both symptoms point at CratonVM's classloading/loader-identity handling
during Hibernate's newer `hibernate-models`-based boot metadata layer
(replacing the older Jandex-direct + hbm-annotation-scanning path in recent
Hibernate ORM versions). The QName CCE is a textbook two-classloader
identity split; the Jandex indexing exception could be a downstream
consequence of the same split (Jandex's own internal indexing walking a
class graph that includes a duplicate/wrongly-loaded type) or an
independent gap in whatever native support `hibernate-models`' Jandex
indexer needs (JAR/classpath resource reading, `DotName`/annotation
metadata construction, etc.).

## Next steps (not yet done)

- Get a full stack trace for the QName CCE to identify exactly which two
  loaders/namespaces produced the divergent `QName` `Class` objects — this
  is the same debugging pattern used successfully for other loader-identity
  bugs in this repo (compare `class_id`/loader namespace of both operands).
- Get a full stack trace for the Jandex indexing exception's *cause* (the
  message is truncated to "Error indexing standard types" with no
  underlying reason visible in the harness's 160-char capture).
- Confirm CratonVM-specificity with a HotSpot run (pending) — Jandex/JAXB
  XML processing has real complexity even on HotSpot, so this cluster is a
  reasonable candidate to actually reproduce on HotSpot too; don't assume
  CratonVM-specific without confirmation, unlike the more clearly
  CratonVM-internal clusters in this audit.
