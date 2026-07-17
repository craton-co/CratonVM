# `Archive`/`Launcher` classpath URL enumeration returns empty or wrong results

**Status: OPEN — found 2026-07-17**

## Symptom

| Class | Failing tests | Note |
|---|---:|---|
| `org.springframework.boot.loader.launch.JarFileArchiveTests` | 7 of 10 | all via `archive.getClassPathUrls(...)` |
| `org.springframework.boot.loader.launch.JarLauncherTests` | 4 of 5 | via `launcher.getClassPathUrls()` / `createClassLoader(...).getURLs()` |
| `org.springframework.boot.loader.launch.WarLauncherTests` | 4 of 4 | same, WAR/`WEB-INF` layout |
| `org.springframework.boot.loader.launch.PropertiesLauncherTests` | 3 of 14 | same shape, rest is a separate cluster (see below) |
| `org.springframework.boot.loader.launch.ExplodedArchiveTests` | 1 of 7 | related but narrower — see "Related but distinct" |

Representative failure (`JarFileArchiveTests.getClassPathUrlsWhenNoPredicatesReturnsUrls`):

```
=> org.opentest4j.AssertionFailedError:
Expecting actual:
  []
to contain exactly (and in same order):
  [jar:nested:/.../root.jar/!META-INF/!/,
    jar:nested:/.../root.jar/!META-INF/MANIFEST.MF!/,
    jar:nested:/.../root.jar/!1.dat!/,
    jar:nested:/.../root.jar/!2.dat!/,
    jar:nested:/.../root.jar/!d/!/,
    jar:nested:/.../root.jar/!d/9.dat!/,
    jar:nested:/.../root.jar/!special/!/,
    jar:nested:/.../root.jar/!special/ë.dat!/,
    jar:nested:/.../root.jar/!nested.jar!/,
    jar:nested:/.../root.jar/!another-nested.jar!/,
    jar:nested:/.../root.jar/!space nested.jar!/,
    jar:nested:/.../root.jar/!multi-release.jar!/]
```

`WarLauncherTests`/`JarLauncherTests`/`PropertiesLauncherTests` show the exact
same "actual: []" shape, even for fixtures that use a genuine
`BOOT-INF/classes` + `BOOT-INF/lib/*.jar` layout (e.g.
`JarLauncherTests.archivedJarHasOnlyBootInfClassesAndContentsOfBootInfLibOnClasspath`),
so this is not purely a "wrong layout assumed" problem — see "Root cause"
below for the confirmed vs. unconfirmed parts.

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader.org.springframework.boot.loader.launch.JarFileArchiveTests.out.log`
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader.org.springframework.boot.loader.launch.JarLauncherTests.out.log`
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader.org.springframework.boot.loader.launch.WarLauncherTests.out.log`
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader.org.springframework.boot.loader.launch.PropertiesLauncherTests.out.log`

## Not a recurrence of the retired whole-module AIOOBE bug

This module was previously the site of a whole-module
`ArrayIndexOutOfBoundsException` in `FileDataBlock$FileAccess.read`'s bulk
`ByteBuffer.put` (fixed 2026-07-12, see
`docs/internal/springboot/zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md`).
None of the failures in this doc show that exception, that call site, or any
`ScopedMemoryAccess`/`copyMemory` trace — the AIOOBE fix's own "known
residuals" section in fact already flagged
`ExplodedArchiveTests.getClassPathUrlsWhenNoPredicatesReturnsUrls` (the
narrower one, see below) as a *separate*, not-yet-investigated bug when it
re-ran the module on 2026-07-12. This doc is that follow-up investigation,
five days and one confirmed cause later, now covering the whole classpath-URL
family it turned out to belong to.

## Root cause

**Confirmed, file:line-precise, for `JarFileArchive`:**
`org/springframework/boot/loader/launch/JarFileArchive.getClassPathUrls(Predicate,Predicate)`
is intercepted by a CratonVM native,
`p59_spring_boot_jar_archive_get_class_path_urls`
(`native-builtins/src/phases_late.rs:23008`, registered at
`native-builtins/src/phases_late.rs:22119-22124`). It exists specifically to
avoid depending on `Stream.map/filter/collect` + `Collectors.toCollection`
(the native's own doc comment, `phases_late.rs:23001-23007`, says CratonVM's
synthetic `Stream` "does not support arbitrary `Function`/`Predicate` lambdas
yet"). Its implementation,
`p59_fat_jar_boot_inf_nested_url_values` (`phases_late.rs:22956-22995`), is
**hardcoded** to two things only: it always appends one
`jar:nested:/<path>/!BOOT-INF/classes/!/` URL (unconditionally — regardless
of whether that entry exists), then scans the archive's central directory for
entries matching literally `BOOT-INF/lib/*.jar`. **Both caller-supplied
`Predicate` arguments are completely ignored**, and no other archive layout
(plain root-level entries as `JarFileArchiveTests` uses, or `WEB-INF/*` as
`WarLauncherTests` uses) is ever considered. `ExplodedArchive` (used by the
exploded-directory variants of these same tests) has **no native override at
all** — confirmed via `grep -r ExplodedArchive native-builtins/src`, zero
hits — so it always falls through to the real JDK bytecode's
`Stream`-based pipeline, which is the exact thing this native was written to
route around for `JarFileArchive`.

This fully explains `JarFileArchiveTests` (whose fixtures are root-level
`1.dat`/`2.dat`/`nested.jar`/etc., never `BOOT-INF/*`, so the native's
`BOOT-INF/lib/*.jar` scan always finds nothing) and `WarLauncherTests`
(`WEB-INF/*`, not `BOOT-INF/*`, for a class that also has no native at all —
`WarLauncherTests` uses `WarLauncher`/`ExplodedArchive`/`JarFileArchive`
depending on the test, but the WAR layout is never matched by the
`BOOT-INF`-only scan either way).

**Unconfirmed:** it does *not* fully explain why
`JarLauncherTests.archivedJarHasOnlyBootInfClassesAndContentsOfBootInfLibOnClasspath`
— whose fixture genuinely uses `BOOT-INF/classes` + `BOOT-INF/lib/{foo,bar,baz}.jar`,
exactly what the native's hardcoded scan looks for — still returns a fully
empty set (0 elements) rather than at least the always-appended
`BOOT-INF/classes/` URL. That test calls `JarLauncher.getClassPathUrls()`
(inherited from `Launcher`, real bytecode) rather than
`JarFileArchive.getClassPathUrls(Predicate,Predicate)` directly; `Launcher`'s
own bytecode resolves `getArchive().getClassPathUrls(filter1, filter2)`
through an `invokeinterface` on the `Archive` interface type. Whether that
particular call shape reaches the same native-override check as a direct
`invokevirtual` call (both `vm/src/vm/vm_exec.rs::invoke_on_class_shared_inner`
and `vm/src/runtime/interpreter.rs`'s own dispatch step 6, around line 26773,
implement *different* native-override gating logic — the former requires an
explicit class+method allowlist entry that does not exist for the bare
`java/util/zip/ZipFile`/`java/util/jar/JarFile` case documented separately in
`spring-boot-loader-zipfile-close-invokespecial-native-bypass-npe.md`, while
the latter appears unconditional for non-interface declaring classes) was not
traced to a definitive answer in this pass. The strongest hypothesis is that
some downstream step in `Launcher.getClassPathUrls()` (a `LinkedHashSet`
`addAll(...)` over the returned synthetic 2-field `ArrayList`-shaped-as-`Set`
object the native returns, per `phases_late.rs:23049-23056`'s own comment
that the return type mismatch is deliberate) silently drops every element —
this would need a live repro with `CRATONVM_DBG_SBLOAD=1` (the native's own
debug flag) to confirm whether the native fires at all and what it returns
before the `Launcher`-level wrapping.

## Related but distinct: `ExplodedArchiveTests.getClassPathUrlsWhenNoPredicatesReturnsUrls`

This one test (unlike everything above) returns a **mostly-correct**, non-empty
result — it's missing exactly 3 specific entries (`META-INF/MANIFEST.MF`,
the nested-directory file `d/9.dat`, and the percent-encoded-filename entry
`special/ë.dat`) rather than being empty. Since `ExplodedArchive` has no
native override, this is the real-JDK-bytecode directory-walk path running
end-to-end but under-enumerating — most likely a recursion-depth or
filename-encoding gap in whatever CratonVM real-NIO directory-listing
support backs it, not the same BOOT-INF-hardcoding mechanism above. This was
already flagged as an unexplored residual in the 2026-07-12 AIOOBE fix doc's
"known residuals" section and is still unexplained; not chased further here
because it did not fit either confirmed mechanism above.

## Affected classes

| module | class |
|---|---|
| loader/spring-boot-loader | org.springframework.boot.loader.launch.JarFileArchiveTests |
| loader/spring-boot-loader | org.springframework.boot.loader.launch.JarLauncherTests |
| loader/spring-boot-loader | org.springframework.boot.loader.launch.WarLauncherTests |
| loader/spring-boot-loader | org.springframework.boot.loader.launch.PropertiesLauncherTests |
| loader/spring-boot-loader | org.springframework.boot.loader.launch.ExplodedArchiveTests |
