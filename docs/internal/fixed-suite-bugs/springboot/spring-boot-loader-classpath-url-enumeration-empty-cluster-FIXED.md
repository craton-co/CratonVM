# `Archive`/`Launcher` classpath URL enumeration returns empty or wrong results — FIXED

**Status: FIXED 2026-07-19.** Originally opened 2026-07-17 as
`docs/known-issues/springboot/spring-boot-loader-classpath-url-enumeration-empty-cluster.md`;
retired here per the known-issues triage rule (primary defect fixed; the one
remaining residual is a previously-existing, separately-tracked cluster —
see "Residual" below).

## Original symptom (2026-07-17)

| Class | Failing tests | Note |
|---|---:|---|
| `org.springframework.boot.loader.launch.JarFileArchiveTests` | 7 of 10 | all via `archive.getClassPathUrls(...)` |
| `org.springframework.boot.loader.launch.JarLauncherTests` | 4 of 5 | via `launcher.getClassPathUrls()` / `createClassLoader(...).getURLs()` |
| `org.springframework.boot.loader.launch.WarLauncherTests` | 4 of 4 | same, WAR/`WEB-INF` layout |
| `org.springframework.boot.loader.launch.PropertiesLauncherTests` | 3 of 14 | same shape; the other 11 were always a separate cluster, see below |
| `org.springframework.boot.loader.launch.ExplodedArchiveTests` | 1 of 7 | related but narrower |

Root cause (confirmed 2026-07-17): `JarFileArchive.getClassPathUrls` was
intercepted by a CratonVM native
(`p59_spring_boot_jar_archive_get_class_path_urls`,
`native-builtins/src/phases_late.rs`) that was **hardcoded** to only two
things — always appending a `BOOT-INF/classes/!/` URL and scanning for
`BOOT-INF/lib/*.jar` — completely ignoring both caller-supplied `Predicate`
arguments and any non-BOOT-INF archive layout (plain root-level entries,
`WEB-INF/*`). `ExplodedArchive` had **no native override at all**, so it fell
through to the real-JDK bytecode `Stream` pipeline, which itself had a
separate, narrower under-enumeration bug.

## Fix (2026-07-19, branch `codex/fix-springboot-loader-classpath-enumeration-20260718-019f7681`)

1. **`p59_spring_boot_jar_archive_get_class_path_urls` rewritten**
   (`native-builtins/src/phases_late.rs`) to walk the jar's actual central
   directory, wrap each entry in the real `JarFileArchive$JarArchiveEntry`,
   evaluate the caller's real include `Predicate` via `invoke_virtual`, and
   delegate URL construction to the real `JarFileArchive.getNestedJarUrl`
   bytecode via `invoke_special_bytecode_only` — instead of a hardcoded
   BOOT-INF-only scan. Works for plain root-level jars, `BOOT-INF/*` fat
   jars, and (via the launcher's own filters) `WEB-INF/*` WARs alike.

2. **New native for `ExplodedArchive.getClassPathUrls`**
   (`p59_spring_boot_exploded_archive_get_class_path_urls`) — previously
   unhandled. Walks the exploded directory tree natively, building the same
   `FileArchiveEntry` shape the real bytecode uses and evaluating both the
   include filter and the directory-search filter, instead of relying on the
   real-JDK `LinkedList.addAll(0, ...)` path that was silently dropping every
   descendant of an immediate directory (manifests, nested files,
   percent-encoded names).

3. **`ExecutableArchiveLauncher.createClassLoader(Collection)` native
   simplified** to re-enter the concrete real bytecode via
   `invoke_special_bytecode_only` with the caller-supplied URL collection
   (preserving its classpath-index merge/ordering), instead of rebuilding a
   URL array from the same hardcoded BOOT-INF-only scan as fix #1.

4. **Supporting fixes** needed for the above to round-trip correctly:
   - `JarEntry.getComment()` now returns the real central-directory comment
     (used for the `UNPACK:` marker on nested-jar entries) instead of always
     `null`; the synthetic `JarEntry` layout gained a 5th slot for it.
   - `classloader.rs`'s `cl_load_class_base_delegation` now probes
     `ucl_try_define_local_class` for **any** classloader, not only ones
     `object_extends` reports as a direct `URLClassLoader` subclass — some
     real-JDK subclasses (e.g. Spring Boot's `LaunchedClassLoader`) don't
     surface that relationship through native dispatch. The helper itself is
     a no-op for loaders with no recorded URLs, so this is safe.
   - `extract_url_path` now reads a `URL`'s real by-name `file`/`path`
     fields (falling back to the old synthetic numeric slots), and
     normalizes Spring Boot 3's `nested:` URL scheme
     (`jar:nested:/<outer>/!<entry>` → `/!` becomes `!/`) — previously only
     the synthetic slots were read, which lost constructor URLs for ordinary
     `URLClassLoader` instances built from real `URL` objects.
   - `t19_h10_class_manifest_attr` (`lang_class.rs`) gained a
     `spring_boot_exploded_manifest_attr` path: classes loaded from a
     `BOOT-INF/classes` or `WEB-INF/classes` directory now inherit manifest
     attributes from the enclosing archive root's `../../../../apps/META-INF/MANIFEST.MF`,
     matching how a real exploded Spring Boot launch resolves package
     metadata. (Adapted during the `origin/dev` merge below to plug into
     `dev`'s own concurrently-added per-package manifest infrastructure —
     `parse_package_manifest`/`manifest_attr_for_package` — instead of a
     standalone main-attributes-only line scanner, and gained a matching
     `exploded_manifest_cache` for parity with the existing plain/nested-jar
     caches.)

## Verification (2026-07-19)

Built a fresh release binary from the branch and ran each affected class
directly against `SbRunner` (bypassing a suite-runner harness artifact, see
"Harness note" below). First pass (before merging the 97 commits `dev` had
gained since this branch forked on 2026-07-18) showed `JarLauncherTests` at
4/5, with the 5th (`explodedJarDefinedPackagesIncludeManifestAttributes`)
failing on `Class.getPackage().getImplementationTitle()` returning `null` —
confirmed via live trace to be **not** a manifest-resolution bug (this fix's
own `spring_boot_exploded_manifest_attr` path correctly resolved `"test"`),
but a separate, pre-existing gap in `native_class_get_package` writing
manifest attributes onto fields that don't exist on a real JDK 9+ `Package`
object. Merging `origin/dev` in turned out to already carry an independent,
unrelated fix for exactly that gap (`package_version_info`, materializing a
real `Package$VersionInfo` record) — after the merge, `JarLauncherTests`
reached 5/5 with no further changes needed:

| Class | Before (2026-07-17) | After (2026-07-19, post-`dev`-merge) |
|---|---|---|
| `JarFileArchiveTests` | 3/10 PASS | **10/10 PASS** |
| `WarLauncherTests` | 0/4 PASS | **4/4 PASS** |
| `ExplodedArchiveTests` | 6/7 PASS | **7/7 PASS** |
| `JarLauncherTests` | 1/5 PASS | **5/5 PASS** |
| `PropertiesLauncherTests` | 11/14 PASS (of the 14 in this cluster's original scope) | **28/32 PASS** (of the full class; residual is the pre-existing, separately-tracked cluster below) |

### Harness note

The suite runner (`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`)
intermittently reported `JarFileArchiveTests` as `CRASH` (`rc=-1`, zero
stdout/stderr) or `HANG` when run through its hidden-window
(`CreateNoWindow=true`, redirected stdio) `.NET Process` invocation, at
~25-28s into what is actually a real, CPU-bound ~140s run (the class's
`writeZip64Jar` fixture helper builds 65,537 zip entries via
`JarOutputStream`/`Deflater` before the test under investigation even runs).
A direct foreground invocation of the identical binary and classpath
completed normally and passed every time. Not investigated further —
flagged as a runner-harness quirk (possibly AV/EDR flagging a
hidden-window, sustained-high-CPU child process on this host), not a VM
correctness bug. If this resurfaces, prefer running the affected class
directly via `SbRunner`/a single-method runner rather than trusting the
suite runner's own status classification for it.

## Residual (tracked separately, per triage rule) — now also FIXED

- **`propertieslauncher-loader-path-ignored-wrong-app-launched.md`**
  — the pre-existing, separately-root-caused 11-test cluster in
  `PropertiesLauncherTests` (`loader.path` silently not applied). This fix
  happened to repair 8 of those 11 as a side effect (better `URL`/classpath
  resolution generally). The remaining 4 (`testUserSpecifiedNestedJarPath`,
  `testUserSpecifiedClassLoader`, `classPathWithoutLoaderPathDefaultsToJarLauncherIncludes`,
  `testUserSpecifiedClassPathOrder`) were fixed 2026-07-20 — two independent
  bugs (a naive global string-replace corrupting directory-shaped nested jar
  URLs, and real-JDK-mode `ClassLoader.loadClass` never delegating to a
  user-defined parent). Full class now 32/32 PASS. See
  [`propertieslauncher-loader-path-ignored-wrong-app-launched-FIXED.md`](propertieslauncher-loader-path-ignored-wrong-app-launched-FIXED.md).

## Affected classes (original)

| module | class |
|---|---|
| loader/spring-boot-loader | org.springframework.boot.loader.launch.JarFileArchiveTests |
| loader/spring-boot-loader | org.springframework.boot.loader.launch.JarLauncherTests |
| loader/spring-boot-loader | org.springframework.boot.loader.launch.WarLauncherTests |
| loader/spring-boot-loader | org.springframework.boot.loader.launch.PropertiesLauncherTests |
| loader/spring-boot-loader | org.springframework.boot.loader.launch.ExplodedArchiveTests |
