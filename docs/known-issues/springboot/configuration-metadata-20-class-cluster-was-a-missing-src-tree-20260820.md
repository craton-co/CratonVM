# The 20-class configuration-metadata cluster was a missing `src/` tree, not a VM defect

**Status: RESOLVED — 19 of 20 pass on CratonVM once the fixture is repaired,
with test counts matching HotSpot exactly. The 20th fails identically on
HotSpot. Zero CratonVM defects in this cluster.**

The 3-GC re-run of the 104 non-passing Spring Boot classes had 20 classes in
`configuration-metadata/` FAILing on **all three collectors** — 17 in
`spring-boot-configuration-processor`, 3 in
`spring-boot-configuration-metadata-changelog-generator`. A cluster that large,
failing identically under every collector, reads like one shared VM defect.

It was not a VM defect at all.

## What it was

```
java.lang.IllegalStateException: Unable to read content
  org.springframework.core.test.tools.SourceFile.forTestClass(SourceFile.java:72)
Caused by: java.io.FileNotFoundException:
  src\test\java\org\springframework\boot\configurationsample\generic\SimpleGenericProperties.java
```

These are annotation-processor tests: they read their **own `.java` sources at
runtime** and compile them on the fly. The path is relative, so it resolves
against the module root — and the module root in the local fixture contains only
`build/`. There is no `src/` directory.

**132 of the fixture's 139 module directories DO have `src/`.** Seven do not,
and both cluster modules are among them:

```
core/spring-boot
module/spring-boot-autoconfigure-classic
module/spring-boot-autoconfigure-classic-modules
module/spring-boot-test-classic-modules
configuration-metadata/spring-boot-configuration-metadata
configuration-metadata/spring-boot-configuration-metadata-changelog-generator
configuration-metadata/spring-boot-configuration-processor
```

So this is a per-module copy gap in one fixture, not a property of the fixture
design. The compiled `configurationsample` **classes** are present under
`build/classes/java/test/`, which is why nothing else noticed: everything except
the source-reading tests works fine.

## The HotSpot arm said so immediately

Before the repair, HotSpot failed exactly as CratonVM did — `TypeUtilsTests`
5/5 failed, `PropertyDescriptorResolverTests` 16/16 failed, same
`FileNotFoundException`, same path. **That alone disqualified the cluster as a
VM defect**, and it cost one command. Running it first would have saved the
whole triage.

## Repair

The Azure fixture (`/data/cratonvm/apps/spring-boot`) has the sources for all
seven modules. Copied the two cluster modules down as a tarball (253 `.java`
files; a tarball, not per-file `scp`, so no CRLF translation) and extracted into
the local fixture. `apps/` is `.gitignore`d, so this is a local fixture repair
and not a repo change — it has to be redone on any fresh fixture.

## After the repair

| arm | result |
| --- | --- |
| HotSpot, 20 classes | 19 pass, `ChangelogWriterTests` 1/1 fails |
| CratonVM, 20 classes | 19 pass, `ChangelogWriterTests` 1/1 fails |

**Test counts match HotSpot on all 20 classes** (65, 14, 1, 14, 6, 1, 4, 2, 17,
10, 21, 14, 10, 6, 16, 5, 1, 1, 1, 1) — so the passes are real, not a suite that
quietly ran fewer tests.

`ChangelogWriterTests.writeChangelog` fails on **both** VMs. Diffing the two
arms' assertion output line by line, the expected and actual text are identical;
the only difference is that CratonVM's stack trace includes the
`AssertionFailedError.<init>` frames HotSpot elides. It is a fixture/expectation
mismatch, not a divergence.

## …and on Linux it is not even that (2026-08-20)

Re-run on the Azure host, where the fixture already ships `src/` for both
modules, all 21 classes (the 20 plus `JsonMarshallerTests`) are clean on **four
arms** — HotSpot, and CratonVM under ZGC, G1 and Generational:

```
hs   pass=21  notpass=0
zgc  pass=21  notpass=0
g1   pass=21  notpass=0
gen  pass=21  notpass=0
```

Per-class test counts are **identical between HotSpot and all three collectors
on all 21 classes**, so none of these greens is a suite that quietly ran fewer
tests.

`ChangelogWriterTests` passes on Linux on every arm, so its Windows failure is
**host-specific, not VM-specific** — an expectation mismatch in generated
Asciidoc text on a CRLF host, failing HotSpot and CratonVM alike. It is not a
CratonVM defect on either platform.

One benign count difference between hosts:
`ConfigurationMetadataAnnotationProcessorTests` runs **66** tests on Linux and
**65** on Windows — on *both* VMs, so it is an OS-gated test, not a divergence.

## Carry-over

The other five source-less modules have the same latent gap — `core/spring-boot`
is the big one (969 `.java` files) and contributes many rows to the non-passing
list. Any of its tests that read their own sources will be failing for this
reason and not a VM one. Repair the fixture before triaging them.

**Check `[ -d <module>/src ]` before opening any Spring Boot failure whose
message is a `FileNotFoundException` on a relative `src/...` path.**
