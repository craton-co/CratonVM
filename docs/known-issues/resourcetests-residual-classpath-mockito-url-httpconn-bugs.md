# `ResourceTests` residuals — 5 distinct pre-existing bugs beyond the reported cluster

Status: open

Date observed: 2026-07-03

## Summary

The `core` bug-cluster report listed `org.springframework.core.io.ResourceTests`
as one failing class (`AssertionError`, generic). Root-causing it surfaced
**several** independent bugs, most of which are now fixed (see "Fixed as part
of this pass" below). Getting the class fully to `OK` uncovered 5 further,
unrelated, pre-existing bugs — each masked behind the runner's failcause
display (only ~5 distinct failcauses surface per run even though more test
methods fail; fixing one bug reliably reveals the next). These 5 residuals
are documented here rather than chased further, since each is an unrelated,
separate root cause and the original cluster's reported symptoms are already
resolved.

## Fixed as part of this pass (for context — see commit history, not repeated here)

- `FileChannel.open` / `FileSystemProvider.newFileChannel` (native-builtins
  `phases_late.rs`) mapped a missing file to a bare `IOException` instead of
  `NoSuchFileException`.
- `Files.readAllBytes`/`readString` (native-builtins `phases_late.rs`, two
  call sites each) had the same bug.
- `Files.readAttributes`'s plain-filesystem fallback (native-builtins
  `phases_late.rs`) silently returned a fake all-zero `BasicFileAttributes`
  on ANY error, including a missing file — masking every NIO
  attribute-read failure, not just this test's. Real bytecode
  `Files.getLastModifiedTime`/`size()`/etc. are thin wrappers over
  `readAttributes`, so this one fix covers all of them.
- `build_synthetic_url` (native-builtins `jboss_module_loader.rs`) wrote the
  full URL spec string into the real `authority` field (dead code left over
  from a since-fixed `URL.equals`/`hashCode` implementation that used to key
  off it) — corrupting relative URL resolution
  (`new URL(context, spec)`/`URLStreamHandler.parseURL`'s authority-
  inheritance) with "Illegal character found in authority" whenever a
  synthetic classpath-resource URL was used as a relative-resolution
  context.
- `AbstractApplicationContext.getEnvironment()` (native-builtins
  `spring_startup_bootstrap.rs`) called `construct_real_standard_environment`
  directly instead of dispatching virtually to `createEnvironment()`,
  breaking web-context `StandardServletEnvironment` overrides — this is the
  `EnvironmentSystemIntegrationTests` fix, unrelated to `ResourceTests` but
  landed in the same pass.

## Residual 1 — `ClassPathResource.createRelative("../X.class")` fails `getURL()`

```
FAILCAUSE ... resourceCreateRelativeWithDotPath [ClassPathResource with Class] ::
java.io.FileNotFoundException: class path resource
  [org/springframework/core/io/../CollectionFactoryTests.class]
  cannot be resolved to URL because it does not exist
```

`ClassPathResource(String, Class)`'s constructor DOES call
`StringUtils.cleanPath(path)` on the raw `path` field — but the class-package
prefix is concatenated onto `absolutePath` *after* that cleaning, so a
`createRelative("../CollectionFactoryTests.class")` call (whose `this.path`
is just the bare filename, no directory, since the resource was originally
constructed as `new ClassPathResource("ResourceTests.class", ResourceTests.class)`)
produces a raw, uncleaned `absolutePath` = `"org/springframework/core/io" + "/" +
"../CollectionFactoryTests.class"`. This is expected/by-design Spring
behavior — the code comment on `is_directory_resolvable_resource_name` in
`classloading/src/class_path.rs` confirms: *"HotSpot normalizes '.' and '..'
when a URLClassLoader probes a directory classpath root"*, i.e. the JDK's
`ClassLoader.getResource()` is expected to tolerate the embedded `..` for
directory classpath roots, and CratonVM's equivalent does not (yet) do so
for the URL-returning path specifically. `find_resource`/
`find_all_resource_urls` (`classloading/src/class_path.rs`) both already do a
live `full_path.exists()` + `canonicalize` check (which *should* transparently
resolve `..` at the OS level) — so the bug is likely in whatever function
specifically backs the *singular* `Class.getResource(String)` /
`ClassLoader.getResource(String)` URL construction, not the two
already-examined functions. Not root-caused to a specific file:line yet.

## Residual 2 — `getFilePath()` : Mockito cannot mock `java.nio.file.Path`

```
org.mockito.exceptions.base.MockitoException:
Mockito cannot mock this class: interface java.nio.file.Path.
Underlying exception : java.lang.IllegalArgumentException: object of type
  net.bytebuddy.description.type.TypeDescription$Generic$AnnotationReader$NoOp
  is not an instance of java.lang.reflect.AnnotatedType
```

A ByteBuddy `AnnotatedType`/`TypeDescription$Generic$AnnotationReader`
compatibility gap, same family as other previously-documented
JSpecify/`AnnotatedType` reflection gaps in this codebase (search
`docs/known-issues`/`docs/internal` for "AnnotatedType" /
"jspecify-typeuse"). Not re-investigated in this pass.

## Residual 3 — `urlAndUriAreNormalizedWhenCreatedFromFile()`

```
Expecting actual:
  "file:/C:/craton/cratonvm/apps/spring-suite-runner/java.nio.file.Path@13653ec9"
to match pattern:
  "^file:\/[^\/].+test1\.txt$"
```

A `Path` object's default (identity-hashcode-style) `toString()` —
`java.nio.file.Path@13653ec9` — is embedded literally into a constructed
URL string somewhere in `FileSystemResource`/`UrlResource` URL-building,
instead of the real resolved path string. Likely a missing/incomplete
`Path.toString()` override or a code path that calls `.toString()` on a
`Path` whose real bytecode-visible `toString()` isn't wired up the way a
real `sun.nio.fs.WindowsPath` would be. Not root-caused in this pass.

## Residual 4 — `canCustomizeHttpUrlConnectionForExistsFallback()`

```
expected: "Spring"
 but was: null
```

`UrlResource`'s `exists()` fallback path lets a subclass customize the
`HttpURLConnection` before checking existence; the customization isn't
taking effect (or the connection being checked isn't the customized one).
Not root-caused in this pass — a different subsystem (HTTP/URLConnection)
from the other 3 residuals.

## Residual 5 — `[6] FileSystemResource with File path` : `lastModified()` doesn't throw for a missing relative file

```
FAILCAUSE ... resourceCreateRelativeUnknown [FileSystemResource with File path] ::
java.lang.AssertionError: Expecting code to raise a throwable.
  at ResourceTests.resourceCreateRelativeUnknown(ResourceTests.java:125)  // relative4::lastModified
```

This argset constructs the resource via
`new FileSystemResource(Paths.get(resourceClass.toURI()))` (a `Path`, not a
`String`/`File`). Three real fixes landed while chasing this (each
independently correct and kept):

- `FileChannel.open`/`newFileChannel`, `Files.readAllBytes`/`readString`, and
  `Files.readAttributes`'s plain-filesystem fallback (previously silently
  faked an all-zero `BasicFileAttributes` for ANY error, including a missing
  file — this alone was a real, broad correctness bug affecting every
  `Files.getLastModifiedTime`/`size()`/etc. caller) all now correctly map
  ENOENT to `NoSuchFileException`.
- `extract_path_string` (native-builtins `phases_late.rs`) was missing the
  `p57_to_os_path` conversion that `p57_read_path` already had (stripping the
  leading `/` from a `/C:/...` Windows drive path) — this made
  `Files.readAttributes`/`getLastModifiedTime` resolve a *different* string
  than `Files.exists()` for the exact same logical path. Fixed.

None of these individually fixed *this* argset. The `Path`-based
`FileSystemResource.createRelative()` (spring-core's
`FileSystemResource.java:373-377`) does
`new FileSystemResource(this.filePath.getFileSystem(), pathToUse)` — going
through `Path.getFileSystem()` → `FileSystem.getPath(String)` (an *instance*
method on the `FileSystem` object returned by the original `Path`), which is
a different native code path than the URI-based
`FileSystemProvider.getPath(URI)` used by the original `Paths.get(uri)`
construction (that one is correctly `p57`-shaped — verified by reading
`native-builtins/src/phases_late.rs` around the `fsp.getPath(URI)`
registration). The `FileSystem.getPath(String, String...)` instance-method
path was not traced to a specific file:line before time ran out on this
investigation; symptomatically the resulting relative `Path`'s string
representation resolves to something that makes `std::fs::metadata(...)`
report success (or an error kind other than `NotFound`) instead of failing,
so `lastModified()` returns a bogus value instead of throwing.

## Why not fixed here

Each residual is an independent root cause in a different subsystem
(classpath resource resolution, Mockito/ByteBuddy reflection, Path/URL
string handling, HttpURLConnection customization hooks, `FileSystem.getPath`
instance-method plumbing). The cluster's originally-reported failcauses
(FileSystemResource `AssertionError`, UrlResource `MalformedURLException`)
are resolved for 2 of 3 `FileSystemResource` construction variants (String,
File) and for `UrlResource`; chasing all 5 of these newly-surfaced, unrelated
bugs to completion was judged out of scope for one bug-cluster pass.
Documenting each with enough detail to pick up independently.

## Suggested next steps for Residual 5

Trace `java.nio.file.FileSystem.getPath(String, String...)` (instance
method, NOT `FileSystemProvider.getPath(URI)`) — find its native
registration (search `native-builtins/src/phases_late.rs` for
`"java/nio/file/FileSystem"` + `"getPath"`, distinct from the
`FileSystemProvider`/`fsp` variable's URI-based registration already
verified correct) and confirm what `Path` object shape it produces, and
whether that shape's field-0 string is what `extract_path_string`/
`p57_read_path` expect.
