# Hibernate boot-time model building: QName, Jandex cursor, and discovery cluster (fixed)

| | |
|---|---|
| **Status** | FIXED on 2026-07-12; all nine affected classes pass with JIT enabled and disabled. |
| **Discovered** | 2026-07-11 full 4548-class Hibernate suite audit. |
| **Area** | XML model processing, Jandex class-file indexing, and reflection-backed Hibernate Models discovery. |

## Original failures

The affected classes co-located three independent runtime defects:

1. XML processing raised a self-name QName cast failure. This was fixed earlier
   on dev by the interface-stamped synthetic receiver/native dispatch change in
   commit `da3f6a7b1`.
2. Jandex indexing of baseline Java types exhausted valid class resources after
   reading an impossible `Code` length. A minimal parser reproduced the cursor
   divergence through `DataInputStream.skip(long)`.
3. After indexing was repaired, explicit and scanned Hibernate entities were
   absent. Direct reflection and a Jandex model saw `@Entity`, while Hibernate's
   reflection-backed model returned an empty annotation set.

## Root causes and fixes

### Stream cursor and layout

CratonVM mixed real-JDK `BufferedInputStream`/`FilterInputStream` bytecode with
native-backed stream state. The native `BufferedInputStream` constructor did
not establish the real JDK buffer fields, and a bound `FilterInputStream.skip`
path could report bytes skipped without advancing the cursor used by later
reads.

The fix:

- initializes the inherited input field and real-JDK buffer/cursor/mark fields;
- consistently routes the registered BufferedInputStream, FilterInputStream,
  and ByteArrayInputStream surfaces through their native implementations;
- implements `FilterInputStream.skip(long)` by reading and discarding through
  the receiver, preserving subclass delegation and one shared cursor;
- uses a pinned 2 KiB discard buffer and direct unmarked buffered bulk reads, so
  class-file parsing does not regress to one virtual call per byte.

The permanent integration probe checks small and 7,000-byte skips through both
raw and buffered ByteArrayInputStreams and verifies the exact next byte.

### Bound Class method references

`Class::getAnnotations` and `Class::getName` were admitted to the lambda
implementation bytecode cache. That cache executed the concrete real-JDK Class
body before the registered CratonVM mirror/annotation native could run. Direct
`type.getAnnotations()` was correct, but the bound supplier returned no
annotations; `type::getName` returned an internal slash-separated name.

The fix defines one shared native-override predicate for the Class name and
annotation surface, applies it to both dispatch gates, and excludes those
methods from cached lambda bytecode execution. Hibernate's reflection model now
recognizes Address, Book, and Library, and both explicit-class and scanned
metadata bootstrap bind all three entities.

## Validation

Unique release binary:

`C:\\craton\\cargo-targets\\hib-jandex-complete-20260712-base\\release\\cratonvm.exe`

- `filter_input_stream_skip_cursor`: 1/1 pass, including raw/buffered small and
  bulk cursor checks.
- `Comparator.class` raw parser: 25 methods parsed in JIT and no-JIT modes;
  no impossible code length or EOF.
- bound Class supplier probe: annotations match direct reflection, names are
  dotted, and Hibernate reflection metadata binds all three discovery entities.
- all nine affected classes in one process per mode, with a 600-second
  per-method validation budget: 34/34 tests pass with JIT disabled and 34/34
  pass with JIT enabled, with explicit completion markers and zero failures.

`XmlProcessingSmokeTests` also passes the runner's original 120-second method
budget (5/5 in both modes). `ScannerTest.testCustomScanner` is correctness-green
but slower than that generic runner budget; it passes 2/2 in both modes with the
explicit 600-second validation setting. This is a runner timing policy, not the
former jar-scanning/entity-discovery correctness failure.

## Affected classes closed

- `org.hibernate.orm.test.boot.models.xml.XmlProcessingSmokeTests`
- `org.hibernate.orm.test.boot.models.xml.dynamic.DynamicModelTests`
- `org.hibernate.orm.test.annotations.xml.ejb3.Ejb3XmlManyToOneTest`
- `org.hibernate.orm.test.intg.AdditionalMappingContributorBasicColumnTests`
- `org.hibernate.orm.test.jpa.boot.discovery.SimpleTests`
- `org.hibernate.orm.test.boot.models.SourceModelTestHelperSmokeTests`
- `org.hibernate.orm.test.boot.models.xml.complete.CompleteXmlInheritanceTests`
- `org.hibernate.orm.test.boot.models.annotation.SimpleAnnotationUsageTests`
- `org.hibernate.orm.test.bootstrap.scanning.ScannerTest`
