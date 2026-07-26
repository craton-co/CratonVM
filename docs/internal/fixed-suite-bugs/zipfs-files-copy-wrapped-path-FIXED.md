# zipfs `Files.copy()` extraction (Quarkus `ZipUtils.unzip`) — two stacked path-layout bugs — FIXED (2026-07-15)

Status: FIXED, merged to `dev` (`90cc7e73`, `882395cd`). Closes item 3 of
`../../known-issues/keycloak/welcomepagetest-stream-spliterator-zipcopy-residuals-20260715.md`
("`Files.copy()` from a non-default `FileSystemProvider` path fails — REPRODUCED, NOT ROOT-CAUSED").

## Summary

Two independent bugs, both in the same family (a native blindly assumes CratonVM's own synthetic
`Path`/field-index layout for an object that is actually a *different*, real-bytecode `Path`
implementation) combined to make `io.quarkus.fs.util.ZipUtils.unzip()` — the mechanism Quarkus's
Maven/Keycloak-distribution bootstrap (and, by extension, `org.keycloak.testframework.server.
DistributionKeycloakServer.createInstallation()`) uses to extract a zip onto the real filesystem —
fail completely, blocking every Keycloak `tests/base` integration test that needs a running server.

1. **`Files.copy(Path,Path,CopyOption...)` didn't handle a jarfs-encoded SOURCE.**
2. **`p57_read_path()` didn't handle a decorator/wrapper Path for the FileSystem-mount call**, which
   is actually the more severe of the two: it silently mounted the REAL HOST FILESYSTEM ROOT instead
   of the zip, and a `Files.walkFileTree` proceeded to try to copy the *entire host filesystem* into
   the extraction target.

## Bug 1: `Files.copy` — jarfs-encoded SOURCE not classified (native-builtins/src/phases_late.rs, `register_phase57_nio_file`, the `Files.copy(Path,Path,CopyOption...)` native)

The native already special-cased a jarfs-encoded **destination** (copying data *into* a mounted
zip/jar — needed for `zipReproducibly`/jar-writing paths) via `jarfs_decode(&dst_path)`, but always
read the **source** through `std::fs::symlink_metadata`/`std::fs::copy`, which treat whatever string
`p57_read_path`/`read_path_str` returned as a literal OS path. For a Path obtained from
`FileSystems.newFileSystem(zipPath, (ClassLoader) null).getPath(...)`, that string is CratonVM's own
jarfs-encoded sentinel string (`JARFS\u{1}<jar>\u{1}<entry>`), never a real OS path — so both
`std::fs::symlink_metadata` and `std::fs::copy` unconditionally failed with a raw
`NotFound` → `IOException: No such file or directory (os error 2)`, even though `Files.isDirectory`/
`isRegularFile`/`isSymbolicLink` on the *same* Path object reported correctly (those three already had
their own `vfs_classify`/`jarfs_classify`-aware natives registered separately, which is what made this
look like a `Path`-object-carries-bad-data bug rather than a `Files.copy`-specific gap).

**Fix**: classify the source with `jarfs_decode`/`jarfs_classify` exactly like the destination already
was, and route file reads through the existing `jarfs_read_entry` helper for both the jar→real and
jar→jar cases. Commit: `e38d6f60` (`fix(native-io): handle jarfs-encoded SOURCE path in Files.copy`).

## Bug 2: `p57_read_path` — decorator/wrapper Path silently treated as "no path" (native-builtins/src/phases_late.rs, `p57_read_path`)

`p57_read_path`, the shared path-string extraction helper used by roughly 100 `java.nio.file.*`
natives (including `FileSystemProvider.newFileSystem(Path,Map)`), unconditionally read field index 0
(`P57_PATH_FIELD`) off *any* `Path` object and `read_string()`'d whatever was there, on the assumption
that field 0 always holds CratonVM's synthetic path String. Real JDK bytecode `Path` implementations
CratonVM doesn't control the layout of break this. Concretely:

Quarkus's real Java code (`io.quarkus.fs.util.FileSystemHelper.ignoreFileWriteability`, called from
`ZipUtils`'s private `newFileSystem(Path, Map)` helper before every zip mount) wraps the zip file's
`Path` in `io.quarkus.fs.util.sysfs.PathWrapper` (extends `io.quarkus.fs.util.base.DelegatingPath`),
whose field 0 is the wrapped **delegate `Path` object**, never a `String`. `read_string()` correctly
refused to misinterpret that object as a `String` (returns `None` — it validates the object's class
before decoding, see `vm/src/vm/vm_exec.rs::read_string`), but the old `p57_read_path` treated that
`None` as "this Path has no backing string" and returned `""`.

That empty jar path fed into `p57_alloc_jar_filesystem`, whose `if jar_path.is_empty() { return fs; }`
early-out meant the returned `FileSystem` object was never marked as jar-mounted at all. The very next
step, `FileSystem.getRootDirectories()`, has an explicit "host FileSystem returns the OS root" branch
for exactly this un-mounted case — so `ZipUtils.unzip()`'s `Files.walkFileTree(root, copyVisitor)`
walked, and tried to copy, **the entire real host filesystem** into the extraction target directory.

This was directly observed while re-testing on the shared build host: extraction ballooned to several
GB in under a minute and exhausted the (already tight, ~81M-free) root partition with
`No space left on device (os error 28)`, and depending on which entry it reached when disk ran out,
sometimes surfaced as `IOException: the source path is neither a regular file nor a symlink to a
regular file` (hit while trying to `Files.copy` a device/socket special file somewhere under the real
`/`) — which is the exact, verbatim error text originally reported (misleadingly) as a `ZipUtils`/
`Files.copy` bug in the source known-issues doc; it was actually this bug wearing bug 1's clothes,
since the two only stack when the real Quarkus code path (`ZipUtils.unzip`, which always goes through
`ignoreFileWriteability`) is exercised — a hand-written repro that calls
`FileSystems.newFileSystem(Path, ClassLoader)` directly (as the original investigation's repro did)
never hits bug 2 at all, because that overload is a *different*, already-correct native and never
passes through `PathWrapper`.

**Fix**: when the fast field-0 read doesn't yield a `String`, fall back to a real virtual dispatch to
`Path.toString()` — every concrete `Path`, including delegating wrappers (via their real bytecode
`DelegatingPath.toString() → delegate.toString()`), implements this correctly regardless of field
layout. This is the same "prefer real virtual dispatch over a native layout assumption for a
non-native-shaped receiver" pattern already used elsewhere in this codebase (Spliterator/Process
natives). Commit: `a43436fc` (`fix(native-io): p57_read_path falls back to toString() for wrapped
Path objects`).

## Verification

- Standalone repro (`FileSystems.newFileSystem(zipPath, (ClassLoader) null)` + `Files.copy` on a
  directory entry, and a full `Files.walkFileTree`-based extraction of the real Keycloak 26.6.1
  distribution zip, 493 files): succeeds; a random 15-file SHA-256 sample byte-for-byte matches a
  Python `zipfile` extraction of the same archive.
- End-to-end: `org.keycloak.tests.welcomepage.WelcomePageTest` (via the `tests/base` integration
  harness) now gets past server bootstrap — Keycloak 26.6.1 actually boots
  (`Keycloak 26.6.1 on JVM ... started in 9.419s. Listening on: http://0.0.0.0:8080`), where
  previously it failed before ever reaching that point.

## Residuals uncovered along the way (not fixed here, out of scope for this fix)

- `remoteAccessNoAdmin()`/`remoteAccessWithAdmin()` (and similarly the other WebDriver-backed
  `WelcomePageTest` methods) still fail with `org.openqa.selenium.json.JsonException: Unable to
  parse: ...` — this is the pre-existing, separately root-caused (not fixed) Stream/Spliterator
  eager-drain bug from item 2 of the source known-issues doc; unrelated to this fix, still open.
- `localAccessNoAdminNorServiceAccount()` now fails at a *different*, later point:
  `java.lang.RuntimeException: Failed to resolve artifact:
  org.keycloak.testframework:keycloak-test-framework-remote-providers` (Maven/Sisu bean-loading
  failure inside `org.keycloak.it.utils.Maven.getArtifact`). This looks like a missing/unresolvable
  local Maven artifact in this environment's `m2-repo`, not a CratonVM defect — not investigated
  further (out of scope; flagged for whoever picks up the Selenium/Stream residual next, since fixing
  that will be needed before this one becomes the next blocker for `localAccess*`/`createAdminUser`/
  `accessCreatedAdminAccount`).

## Environment note (unrelated to the code fix, but blocked verification and is worth recording)

The shared build host's root partition (`/`) was down to ~81M free during this investigation, and
Quarkus's own test-framework `TmpDir.resolveTmpDir()` (`apps/keycloak/test-framework/core/.../util/
TmpDir.java`) hardcodes `/tmp` as its first choice *regardless* of the `-Djava.io.tmpdir` JVM property
(only falls through to `TEMP` env var / the property if `/tmp` doesn't exist as a directory) — so
`-Djava.io.tmpdir=...` on the CratonVM command line does **not** redirect where the Keycloak
distribution gets extracted. Worked around for this investigation by replacing `/tmp/kc-test-framework`
with a symlink into the worktree's own `tmp-kctest/` directory (on the much larger `/data` filesystem).
This is host/test-framework environment friction, not something to fix in CratonVM.
