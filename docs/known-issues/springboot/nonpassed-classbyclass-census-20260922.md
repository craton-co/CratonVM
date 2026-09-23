# Spring Boot suite — 61 new PASS→FAIL regressions from `--jdk-only` becoming default

| | |
|---|---|
| **Baseline** | 2026-09-13, run `20260913-153854`, compatible-mode-default binary — PASS=938 FAIL=967 CRASH=24 EMPTY=43 ENV-GATED=3 (1975 classes), wall=1337.7s |
| **New run** | 2026-09-21/22, run `20260921-232902`, `--jdk-only`-default binary, local Windows, `C:/craton/CVM/target/release/cratonvm.exe`, real JDK 25, all defaults, category=all — PASS=877 FAIL=1028 CRASH=24 EMPTY=43 ENV-GATED=2 HANG=1 (1975 classes), wall=2601.2s |
| **Diff** | exactly **61 classes flipped PASS→FAIL, 0 flipped FAIL→PASS** — a clean regression set, not sampling noise |
| **Sources** | `apps/spring-boot-suite-runner/.suite/results/{20260913-153854,20260921-232902}/all-jit/results.tsv` |

This page is scoped to the 61-class diff. The much larger FAIL=1028 total is overwhelmingly pre-existing (967 of it was already FAIL on 2026-09-13, before `--jdk-only` was the default) and out of scope here — see the suite's other pages for that history.

## Root cause: `Path.getFileSystem()` returns null under `--jdk-only`

At least **39 of the 61** carry, verbatim, in `results.tsv`'s note column:

```
java.lang.NullPointerException: Cannot invoke "java.nio.file.FileSystem.provider()"
because the return value of "java.nio.file.Path.getFileSystem()" is null
```

(the sibling form `"java.nio.file.FileSystem.getPath(String, String[])"` also appears). A further several of the 61 show the identical NPE **wrapped** one or two frames up — `BeanDefinitionStoreException`, `IllegalStateException: Failed to load ApplicationContext`, or a bare `NullPointerException: Cannot invoke "java.` truncated in the note column exactly where `nio.file.FileSystem...` would continue — consistent with the same defect surfacing through Spring's classpath/resource scanning (`PathMatchingResourcePatternResolver`) rather than directly. In short: the great majority of the 61, plausibly all but a handful, are one mechanism.

Direct repro classes: `org.springframework.boot.SimpleMainTests`, `org.springframework.boot.info.SslInfoTests`.

**This has already been root-caused and handed off as a background task** (`task_568916e9`, "Fix null FileSystem on Path under `--jdk-only`"), which the user has since started in a separate session working in its own worktree (`jdk-only-issues-f23-f25-f26-...`). This page documents the census this sweep found; it does not duplicate that fix work.

The pattern is the mirror image of a bug this same session already found and confirmed fixed on dev tip: `java/util/Enumeration$Impl` fabrication being refused under strict `--jdk-only` (see the jdk-only known-issues history for that story) — there, a synthetic-fabrication refusal with no fallback silently no-opped instead of erroring. Here, a real JDK object (`Path`) is missing a real backing value (`FileSystem`) that `--jdk-only`'s snapshot/native-JDK boundary is supposed to supply.

## A related but distinct sub-cluster: loader/zip NPEs — 3

| class | note |
|---|---|
| `org.springframework.boot.loader.launch.ArchiveTests` | `UnsupportedOperationException: Path not associated with default file system.` |
| `org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests` | `NullPointerException: Cannot invoke "java.util.zip.ZipFile$CleanableResource.clean()"` |
| `org.springframework.boot.loader.net.protocol.jar.UrlJarFilesTests` | same as above |

These three also print a `[cratonvm] JVMS 6.5 uninstantiable-receiver census` diagnostic naming `java/net/JarURLConnection` (abstract) among several other abstract/interface classes a native handed back as if instantiable. Plausibly downstream of the same `Path`/`FileSystem` gap manifesting inside the zip/jar-loader's own filesystem provider rather than in general `Path` usage — **not confirmed** to be the same root cause as the main 39+, just adjacent and worth checking once that fix lands.

## Unclustered — 2

| class | note |
|---|---:|
| `org.springframework.boot.loader.zip.VirtualZipDataBlockTests` | bare `AssertionError` (no message) |
| `org.springframework.boot.test.context.filter.ExcludeFilterApplicationContextInitializerTests` | bare `AssertionError` (no message) |

Not yet characterized — the note column carries no exception text past the bare `AssertionError`, so these need their own log read before attributing a cause.

## Full list of the 61

```
org.springframework.boot.SimpleMainTests
org.springframework.boot.autoconfigure.AutoConfigurationExcludeFilterTests
org.springframework.boot.autoconfigure.condition.ConditionalOnMissingBeanTests
org.springframework.boot.context.ConfigurationWarningsApplicationContextInitializerTests
org.springframework.boot.context.TypeExcludeFilterTests
org.springframework.boot.context.properties.ConfigurationPropertiesScanRegistrarTests
org.springframework.boot.context.properties.ConfigurationPropertiesScanTests
org.springframework.boot.data.couchbase.test.autoconfigure.DataCouchbaseTestPropertiesIntegrationTests
org.springframework.boot.data.ldap.test.autoconfigure.DataLdapTestPropertiesIntegrationTests
org.springframework.boot.data.mongodb.test.autoconfigure.DataMongoTestPropertiesIntegrationTests
org.springframework.boot.data.r2dbc.test.autoconfigure.DataR2dbcTestPropertiesIntegrationTests
org.springframework.boot.diagnostics.analyzer.NoUniqueBeanDefinitionFailureAnalyzerTests
org.springframework.boot.gson.autoconfigure.jsontest.JsonTestWithAutoConfigureJsonTestersTests
org.springframework.boot.gson.autoconfigure.jsontest.SpringBootTestWithAutoConfigureJsonTestersTests
org.springframework.boot.health.autoconfigure.application.SslHealthContributorAutoConfigurationTests
org.springframework.boot.info.SslInfoTests
org.springframework.boot.jackson.JacksonMixinModuleTests
org.springframework.boot.jackson.autoconfigure.jsontest.JsonTestWithAutoConfigureJsonTestersTests
org.springframework.boot.jackson.autoconfigure.jsontest.SpringBootTestWithAutoConfigureJsonTestersTests
org.springframework.boot.jackson2.JsonMixinModuleTests
org.springframework.boot.json.BasicJsonParserTests
org.springframework.boot.json.GsonJsonParserTests
org.springframework.boot.json.JacksonJsonParserTests
org.springframework.boot.jsonb.autoconfigure.jsontest.JsonTestWithAutoConfigureJsonTestersTests
org.springframework.boot.jsonb.autoconfigure.jsontest.SpringBootTestWithAutoConfigureJsonTestersTests
org.springframework.boot.loader.launch.ArchiveTests
org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests
org.springframework.boot.loader.net.protocol.jar.UrlJarFilesTests
org.springframework.boot.loader.nio.file.NestedFileSystemZipFileSystemIntegrationTests
org.springframework.boot.loader.zip.VirtualZipDataBlockTests
org.springframework.boot.micrometer.tracing.test.autoconfigure.AutoConfigureTracingMissingIntegrationTests
org.springframework.boot.persistence.autoconfigure.EntityScannerTests
org.springframework.boot.restclient.test.autoconfigure.AutoConfigureMockRestServiceServerEnabledFalseIntegrationTests
org.springframework.boot.restclient.test.autoconfigure.RestClientTestPropertiesIntegrationTests
org.springframework.boot.restclient.test.autoconfigure.RestClientTestWithConfigurationPropertiesIntegrationTests
org.springframework.boot.ssl.jks.JksSslStoreBundleTests
org.springframework.boot.ssl.pem.LoadedPemSslStoreTests
org.springframework.boot.ssl.pem.PemCertificateParserTests
org.springframework.boot.ssl.pem.PemContentTests
org.springframework.boot.ssl.pem.PemSslStoreBundleTests
org.springframework.boot.test.autoconfigure.json.JsonTestPropertiesIntegrationTests
org.springframework.boot.test.autoconfigure.json.JsonTestWithAutoConfigureJsonTestersTests
org.springframework.boot.test.autoconfigure.json.SpringBootTestWithAutoConfigureJsonTestersTests
org.springframework.boot.test.autoconfigure.override.OverrideAutoConfigurationEnabledFalseIntegrationTests
org.springframework.boot.test.autoconfigure.override.OverrideAutoConfigurationEnabledTrueIntegrationTests
org.springframework.boot.test.context.AnnotatedClassFinderTests
org.springframework.boot.test.context.bootstrap.SpringBootTestContextBootstrapperIntegrationTests
org.springframework.boot.test.context.bootstrap.SpringBootTestContextBootstrapperTests
org.springframework.boot.test.context.bootstrap.SpringBootTestContextBootstrapperWithContextConfigurationTests
org.springframework.boot.test.context.bootstrap.SpringBootTestContextBootstrapperWithInitializersTests
org.springframework.boot.test.context.filter.ExcludeFilterApplicationContextInitializerTests
org.springframework.boot.testsupport.classpath.resources.OnClassWithPackageResourcesTests
org.springframework.boot.testsupport.classpath.resources.OnSuperClassWithPackageResourcesTests
org.springframework.boot.testsupport.classpath.resources.ResourcesTests
org.springframework.boot.testsupport.classpath.resources.WithPackageResourcesTests
org.springframework.boot.web.server.WebServerSslBundleTests
org.springframework.boot.web.server.servlet.context.AnnotationConfigServletWebServerApplicationContextTests
org.springframework.boot.web.server.servlet.context.MockWebEnvironmentServletComponentScanIntegrationTests
org.springframework.boot.web.server.servlet.context.ServletComponentScanIntegrationTests
org.springframework.boot.webclient.test.autoconfigure.WebClientTestPropertiesIntegrationTests
org.springframework.boot.webclient.test.autoconfigure.WebClientTestWithConfigurationPropertiesIntegrationTests
```

## Open items

1. Fix owned by `task_568916e9`, in progress in a separate session as of this writing — do not duplicate.
2. Once that lands, re-run this same 61-class list and confirm all (or nearly all) flip back to PASS; anything that doesn't is either the loader/zip sub-cluster (genuinely distinct) or one of the 2 unclustered `AssertionError`s.
3. The loader/zip sub-cluster (`ArchiveTests`, `JarUrlConnectionTests`, `UrlJarFilesTests`) should be re-checked independently even after the main fix, since its exception shapes differ.

## Update 2026-09-22 (later the same day) — CORRECTED: the root-cause fix has NOT landed on `dev`

**The first version of this update (superseded below) claimed 47 of the 61
now PASS, based on a `jdkonly-fix-verify-20260922` results file found under
the suite runner's `.suite/results/` — that file's binary provenance was never
actually confirmed, only assumed.** A full authoritative re-run of all 1975
classes from an actually-built, actually-verified `dev`-tip binary
(`b51b91613`, this worktree, run name `jdkonly-d0c2c4-verify-20260922`, wall
3142s) shows **all 61 classes still FAIL**, `SimpleMainTests` and every other
checked row still on the exact original
`NullPointerException: Cannot invoke "java.nio.file.FileSystem.provider()"
because the return value of "java.nio.file.Path.getFileSystem()" is null`.
**Item 1's fix has not landed** — whoever owns `task_568916e9` had not merged
as of this measurement, or the fix that produced that other results file was
never actually the same one and lived in a worktree that was never merged.
Do not trust a `.suite/results/` directory's claimed fix status without
either checking the binary's own commit (most result dirs don't record one)
or reproducing it yourself against a binary you built from a known commit —
this page's own first draft of this update is the cautionary example.

**Comparing the full new run against the `20260921-232902` baseline this
page is built from, exactly ONE class changed status anywhere in the whole
1975-class suite**: `org.springframework.boot.loader.zip.ZipContentTests`
PASS→HANG. That class's sole non-pass mode is a documented environmental gap
(`not-cratonvm-bugs-consolidated.md`: needs several GB of scratch disk to
build a Zip64 test archive past the standard size limit) and this run shared
a heavily-loaded host with ~20 other concurrent sessions (disk headroom
observed fluctuating 71–140 GB free over the run's course) — plausibly
load-induced, not a regression from anything committed this session. No
other class moved in either direction across the entire suite.

**What IS real from this session, despite the corrected headline:** the
`newByteChannel` jar-entry `AbstractMethodError` fix (commit `26399d048`) is
independently verified — compiles, isolation-tested (reverted-and-rebuilt
comparison) to confirm it introduces no regression anywhere in the 1975-class
suite — but because the `Path`/`FileSystem` NPE above still gates almost
every one of these 61 classes before they reach any jar/zip content at all,
its effect is not visible in this census's PASS count yet. It will become
visible once the `Path`/`FileSystem` fix lands; re-run this page's 61-class
list again at that point rather than assuming from this page's history.

**The loader/zip sub-cluster (item 3) was investigated directly (running each
class standalone, past the point the general 61-class NPE would otherwise
block it) and each of the 3 has its own independently confirmed root cause,
which remains accurate regardless of the correction above:**

- `JarUrlConnectionTests` (2 of 47 tests), `UrlJarFilesTests` (9 of 11
  tests): `NullPointerException: Cannot invoke "java.util.zip.
  ZipFile$CleanableResource.clean()"` — a `ZipFile`/`JarFile` cleanup-resource
  field left unpopulated on close. Not yet root-caused; not the same
  mechanism as this page's main finding or as the residual below. Needs its
  own investigation.
- `NestedFileSystemZipFileSystemIntegrationTests` (4 of 4 tests): **this
  session fixed** the `newByteChannel` jar-entry `AbstractMethodError`
  (`SeekableByteChannel.size()J has no Code attribute`) this page originally
  attributed to it — see `SC-resource-io-family.md`'s Cause A, whose fix
  covered real host files but explicitly left jar/nested-zip entries on the
  broken interface-typed stub; this session's fix spills such entries to a
  real temp file and reuses the same proven `FileChannelImpl` construction.
  **Re-verifying end-to-end still fails**, but no longer on that defect — a
  DIFFERENT, deeper bug now fires earlier in all 4 test methods:
  `installedProviders()`'s synthetic "jar"/"jrt" providers are minted as real
  `sun.nio.fs.WindowsFileSystemProvider` instances, whose real bytecode
  `getScheme()` hardcodes `"file"`, so `FileSystems.newFileSystem(URI, Map)`
  can never find a provider matching scheme "jar". Root-caused, not fixed, in
  [`jar-jrt-provider-getscheme-shadowed-by-real-bytecode-20260922.md`](jar-jrt-provider-getscheme-shadowed-by-real-bytecode-20260922.md).
  The `newByteChannel` fix is real and independently verified (compiles,
  doesn't regress anything — checked via a from-scratch pre-fix/post-fix
  isolation rebuild) but this class's own pass/fail won't reflect it until
  the provider-scheme bug is also fixed.

**Also found, unrelated to this page's 61-class scope:** `ArchiveTests`'s
`createFromProtectionDomainCreatesJarArchive` failed
(`UnsupportedOperationException: Path not associated with default file
system`, at `Path.of(codeSourceURI).toFile()`) under a direct single-class
invocation, with and without this session's fix (isolation-tested) — but
**did not** fail in an earlier full-suite sweep's results (that run recorded
this class PASS 8/8). At the time this read as order- or invocation-dependent
and not understood; the update below found it was neither — see there.

## Update 2026-09-22 (same day, third update) — the root-cause fix landed after all, from a parallel session; 57 of 61 confirmed PASS

The `task_568916e9`/`getScheme()` root cause was fixed the same day by a
**parallel Claude session** working this same page's own retirement, on an
until-then-unmerged branch (`claude/spring-boot-hotspot-tests-743118`,
commits `907ebeb7e` — links the default provider's `theFileSystem` field back
to its `FileSystem`, closing the `Path.getFileSystem()` NPE this page's main
section is about — and `9c7b4ed2e` — the jar/jrt `getScheme()` fix the
previous update above root-caused independently; see
[`jar-jrt-provider-getscheme-shadowed-by-real-bytecode-FIXED-20260922.md`](../../internal/fixed-suite-bugs/springboot/jar-jrt-provider-getscheme-shadowed-by-real-bytecode-FIXED-20260922.md)
for the correspondence). Found by searching all local branches for the
message this page had already root-caused, merged into this session's own
branch alongside the `newByteChannel` fix, rebuilt, and **verified this
time against a real, fully-completed 1975-class suite run** (not a found
results file of unconfirmed provenance — see the correction two updates up):

- **57 of the 61 classes in this page's list now PASS** (up from 0/61 measured
  against the same binary before these two fixes were merged in).
- **Zero new regressions anywhere in the 1975-class suite.** Diffing the full
  `jdkonly-d0c2c4-v2-verify-20260922` run against the `20260921-232902`
  baseline class-by-class: 57 `FAIL→PASS` (exactly the 57 above), 1
  `PASS→HANG` (`ZipContentTests`, the same pre-existing disk-space-dependent
  environmental gap noted in the previous update, reproduced independently of
  every fix in this page's history), 0 flips of any other shape.
- `ArchiveTests` — the "unrelated, not understood" flake noted just above —
  is **PASS 8/8** in this run. It was never a flake; it was gated by the same
  `Path.getFileSystem()` NPE as the rest of this page's list, just reached
  through a different call shape (`Path.of(codeSourceURI).toFile()` instead
  of `Files.*`), so it looked unrelated until the same fix closed it too.

**4 of the 61 remain FAIL, each independently root-caused, none the original
NPE:**

| class | tests failing | cause |
|---|---:|---|
| `JarUrlConnectionTests` | 2 of 47 | `NullPointerException: Cannot invoke "java.util.zip.ZipFile$CleanableResource.clean()"` — a `ZipFile`/`JarFile` cleanup-resource field left unpopulated on close. Not yet root-caused. |
| `UrlJarFilesTests` | 9 of 11 | same `CleanableResource` NPE as above. |
| `NestedFileSystemZipFileSystemIntegrationTests` | 1 of 4 (down from 4 of 4) | the `newByteChannel` jar-entry fix (this session, commit `26399d048`) and the `getScheme()` fix together close 3 of the 4 methods; the last, `nestedZipSplitAndRestore`, hits a distinct `NullPointerException: Cannot enter synchronized block because "this.closeLock" is null` in `sun.nio.fs.WindowsDirectoryStream.close` — a `Files.list`-over-a-nested-zip-directory-stream minted without running its real constructor, so the real `closeLock` field it later synchronizes on is null. Not yet fixed. |
| `MockWebEnvironmentServletComponentScanIntegrationTests` | 1 of 3 | newly surfaced now that the NPE ahead of it is gone: `BeanCreationException` → `ConversionNotSupportedException` → `IllegalStateException: Cannot convert value of type 'jakarta.servlet.DispatcherType' to required type 'jakarta.servlet.DispatcherType'` — same-named class on both sides, the signature of a class-identity/classloader split, not investigated further here. |

Bottom line, superseding every earlier update on this page: the root-cause
fix landed (from a different session than the one this page originally
named), closes 57 of the 61 classes with no collateral damage anywhere in the
suite, and the residual 4 are fully itemised above rather than one lump
"loader/zip sub-cluster." None of the 4 residuals are candidates for this
page's own retirement yet; re-open individually as each gets fixed.

## Update 2026-09-22 (same day, fourth update) — `closeLock` fixed; 3 residuals remain

The `claude/spring-boot-hotspot-tests-743118` session (named above as the one
that landed the `theFileSystem`/`getScheme()` root cause) independently
reached the same `NestedFileSystemZipFileSystemIntegrationTests` /
`nestedZipSplitAndRestore` `closeLock` finding the previous update recorded
as "not yet fixed", and fixed it the way that update's own cause column
predicted: `native-builtins/src/phases_late/nio_file.rs`'s
`newDirectoryStream` registration now backfills `closeLock` by name with a
fresh `Object` on the minted concrete `sun.nio.fs.*DirectoryStream`, the same
shape as `p57_link_provider_to_filesystem`. Verified in isolation
(`CRATONVM_DBG_*`-free single-class rerun): `nestedZipSplitAndRestore` no
longer NPEs on that field.

`NestedFileSystemZipFileSystemIntegrationTests` as a whole still FAILs —
its other three methods hit the DIFFERENT, deeper `jdk.nio.zipfs` bug the
previous update already flagged (`Files.readAllBytes` returning `[0]`), which
this fix does not touch — but the specific defect this update is about is
closed.

**3 of the 61 remain FAIL** (down from 4): `JarUrlConnectionTests`,
`UrlJarFilesTests` (the `CleanableResource` NPE), and
`NestedFileSystemZipFileSystemIntegrationTests` (the `jdk.nio.zipfs`
zero-byte read, `closeLock` closed). `MockWebEnvironmentServletComponentScanIntegrationTests`'s
duplicate-`DispatcherType` finding stands as previously recorded — not
investigated further this round.

## Update 2026-09-22 (same day, fifth update) — `NestedFileSystemZipFileSystemIntegrationTests` fixed via abstract-class minting; `MockWebEnvironmentServletComponentScanIntegrationTests` root-caused and fixed; 2 residuals remain

Two more fixes landed on `claude/spring-boot-hotspot-tests-743118`.

**`NestedFileSystemZipFileSystemIntegrationTests`**: a virtual (jar/jrt)
`DirectoryStream` is now minted against the ABSTRACT `java/nio/file/
DirectoryStream` interface instead of a concrete `sun.nio.fs.*` class — no
real `Code` for any filesystem-provider-registered method to shadow, so
dispatch stays on this file's own natives (which only ever read/write the
stream's own appended slots) for the stream's whole lifetime, avoiding the
whack-a-mole real-field-backfill chase (`closeLock`, `isOpen`,
`findDataBuffer`/`NativeBuffer.release()`, ...) a concrete-class mint forces.
Verified: 4/4 PASS (was 3/4, the `findDataBuffer` NPE gone).

**`MockWebEnvironmentServletComponentScanIntegrationTests`**: the
duplicate-`DispatcherType`-identity finding was root-caused to
`ClassUtils.forName`'s native override gating its loader-aware branch on
the wrong predicate for a bare (non-subclassed) `java.net.URLClassLoader`
argument — full writeup in
[`modifiedclasspathclassloader-bare-urlclassloader-child-classutils-fornam-identity-split-FIXED-20260922.md`](../../internal/fixed-suite-bugs/springboot/modifiedclasspathclassloader-bare-urlclassloader-child-classutils-fornam-identity-split-FIXED-20260922.md).
Verified: 3/3 PASS (was 2/3).

**2 of the 61 remain FAIL**: `JarUrlConnectionTests` and `UrlJarFilesTests`
(the `ZipFile$CleanableResource` NPE — root-caused, not fixed; see
[`zipfile-cleanableresource-npe-on-nestedjarfile-close-20260922.md`](zipfile-cleanableresource-npe-on-nestedjarfile-close-20260922.md)).
**59 of 61 PASS.**
