# `LoaderHidingResourceTests`: a freshly-written `jar:` filesystem lists zero entries

**Status: OPEN — found 2026-07-17**

## Symptom

All 3 tests in `module/spring-boot-jetty`'s `LoaderHidingResourceTests` fail
— unrelated to the rest of the module's failures (see the sibling doc
`jetty-private-lambda-wrong-receiver-startcontext-recursion-cluster.md` for
the other 11 Jetty classes in this batch):

```
JUnit Jupiter:LoaderHidingResourceTests:listHidesLoaderResources(File)
    => java.lang.AssertionError:
Expecting ArrayList:
  []
to contain:
  ["/assets/image.jpg"]
but could not find the following element(s):
  ["/assets/image.jpg"]
       org.springframework.boot.jetty.servlet.LoaderHidingResourceTests.listHidesLoaderResources(LoaderHidingResourceTests.java:52)

JUnit Jupiter:LoaderHidingResourceTests:getAllResourcesHidesLoaderResources(File)
    => (same shape, empty ArrayList)

JUnit Jupiter:LoaderHidingResourceTests:resolveHidesLoaderResources(File)
    => org.opentest4j.AssertionFailedError: Expecting value to be true but was false
       org.springframework.boot.jetty.servlet.LoaderHidingResourceTests.resolveHidesLoaderResources(LoaderHidingResourceTests.java:75)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-jetty.org.springframework.boot.jetty.servlet.LoaderHidingResourceTests.out.log`

## Root cause (hypothesis, not confirmed against CratonVM source)

`LoaderHidingResourceTests` (test source:
`apps/spring-boot/module/spring-boot-jetty/src/test/java/org/springframework/boot/jetty/servlet/LoaderHidingResourceTests.java`)
builds a tiny in-memory-style "war" purely with `java.util.jar.JarOutputStream`
— several zero-content directory/file entries (`org/`,
`org/springframework/boot/Loader.class`, `assets/image.jpg`, etc., each just
`putNextEntry`'d with no data written) — then opens it as a `jar:` NIO
filesystem (`FileSystems.newFileSystem(warUri, Collections.emptyMap())`) and
asks a Jetty `PathResourceFactory`/`Resource` to `list()`/`getAllResources()`/
`resolve()` entries inside it. All three assertions fail the same way:
the listing comes back **empty** where HotSpot would enumerate the entries
just written, and a direct `resolve("/assets/image.jpg")` presumably also
comes back non-existent (not shown directly, but `list()`/`getAllResources()`
being empty implies the same underlying directory enumeration is broken).

Two candidate mechanisms, neither confirmed at file:line precision this
session:

1. **`ZipFileSystem`/`jar:` provider directory-listing gap.** This project
   has a history of zip/jar-handling bugs in CratonVM's real-JDK-mode
   `java.util.zip`/`java.nio.file.spi.FileSystemProvider` implementation for
   `jar:` URIs specifically around directory entries and freshly-written
   (not-yet-flushed-to-disk-and-reopened, or reopened-in-the-same-process)
   archives — see the already-fixed
   `docs/internal/springboot/zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md`
   and the unmerged `docs/internal/springboot/reference_zipfile_stream_getcomment_res_npe_fixed.md`-style
   fix in memory (`ZipFile.stream()`/`getComment()` NPE). If CratonVM's zip
   central-directory read (via the JDK's `zipfs`/`ZipFileSystem`, real
   bytecode) doesn't see the directory entries this specific
   `JarOutputStream` wrote (e.g. entries with no data, `STORED` vs
   `DEFLATED` size/CRC edge cases, or a stale/cached view of the file
   written moments earlier in the same process), `Files.newDirectoryStream`/
   the NIO listing Jetty's `PathResource` uses would legitimately return
   empty.
2. **Jetty's own `Resource.list()` implementation working correctly against
   a genuinely-empty listing it received from the (broken) filesystem
   layer** — i.e. the defect is entirely below Jetty, in CratonVM's
   `jar:`-URI `FileSystemProvider`, not in anything Jetty-specific. This is
   the more likely locus given `list()`, `getAllResources()`, AND `resolve()`
   all fail identically — a single shared directory-listing/lookup layer
   underneath all three Jetty-level calls.

Not independently root-caused at the source level this session (no live
repro or `javap`/source read of CratonVM's `zipfs`/jar-URI provider was
done for this specific class). Would need a minimal standalone repro:
write a `JarOutputStream` with directory + zero-content-file entries exactly
as this test does, open `jar:` via `FileSystems.newFileSystem`, and directly
call `Files.list()`/`Files.walk()` on the resulting root, comparing to real
JDK 25 to confirm whether the gap is in the filesystem provider or something
Jetty-`PathResourceFactory`-specific.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.servlet.LoaderHidingResourceTests` |
