# `LoaderHidingResourceTests`: `jar:` URI paths lost their archive identity — FIXED

**Status: FIXED 2026-07-18**

## Symptom

All three `module/spring-boot-jetty`
`org.springframework.boot.jetty.servlet.LoaderHidingResourceTests` cases
returned an empty resource list for a freshly-created WAR containing explicit,
zero-content directory and file entries. `resolve("/assets/image.jpg")` also
reported the existing entry as absent.

## Confirmed root cause

The original `JarOutputStream`/zip central-directory hypothesis was only half
right: the VM could mount the archive and enumerate its entries. The failure
was at the conversion used by Jetty 12's `PathResourceFactory`:

1. The factory first calls `FileSystems.newFileSystem(jarUri, ...)`, then
   converts the same normalized `jar:` URI through `Paths.get(URI)`.
2. CratonVM's `Path.of(URI)` discarded `jar:` semantics and constructed a host
   path from the full URI. Consequently `Files.isDirectory` was false and the
   initial `PathResource.list()` returned an empty collection.
3. After preserving the mounted archive and in-jar entry, a second residual
   surfaced. `PathResource.getName()` calls `toAbsolutePath().toString()`.
   One shared absolute-path conversion anchored CratonVM's internal virtual-FS
   sentinel to the host working directory, leaking the sentinel into names
   instead of producing `/assets/image.jpg`.

## Fix

`native-builtins/src/phases_late.rs` now:

- parses file-backed `jar:` URIs in `Path.of(URI)` into the backing archive and
  in-archive entry, returning a jar-FS encoded path with its owning filesystem;
- treats jar/jrt encoded paths as already absolute in the shared
  `p57_absolute_path_string` helper, so all `Path.toAbsolutePath` registrations
  preserve virtual-FS identity and render the JDK-style `/entry` display path.

The focused `vm/tests/jar_filesystem_zero_entry_listing.rs` regression creates
the exact zero-content WAR shape. It verifies `FileSystems.newFileSystem`,
`Paths.get(jarUri)`, root/nested listing, direct existing/missing lookup, and
the `toAbsolutePath().toString()` values in both JIT and `--nojit` executions.

## Validation

Using the unique task binary
`cratonvm-jetty-loaderhidingres-20260718-019f742c.exe` with Eclipse Adoptium
JDK 25.0.3.9:

- focused zero-entry jar-FS probe: PASS in JIT and `--nojit`;
- Spring Boot suite runner, JIT: `LoaderHidingResourceTests` PASS (3/3);
- Spring Boot suite runner, `--nojit`: `LoaderHidingResourceTests` PASS (3/3).

The final runner results are under
`apps/spring-boot-suite-runner/.suite-loaderhidingres-20260718-019f742c/`.
