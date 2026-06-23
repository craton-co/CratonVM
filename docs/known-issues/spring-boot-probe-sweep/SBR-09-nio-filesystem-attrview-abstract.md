# SBR-09 — NIO `FileSystem` / `FileAttributeView` report abstract types

**Status:** 🟠 Open — object-identity cluster (deferred).
**Recommendation:** FIX — concrete-type gap in the NIO file-system provider.

> Investigated 2026-06-22: same cluster as SBR-08/10/11/13 — CratonVM's NIO
> objects carry the public/abstract type instead of the `sun.nio.fs.Windows*`
> concrete class. Subsystem fix; functionally the file ops are correct.
**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probes

`FSEq`, `DirProbe`.

## Symptom

```
FSEq:     pfs cls            = java.nio.file.FileSystem               (HotSpot: sun.nio.fs.WindowsFileSystem)
DirProbe: getFileAttributeView = java.nio.file.attribute.BasicFileAttributeView
                                                                       (HotSpot: sun.nio.fs.WindowsFileAttributeViews$Basic)
```

`FileSystems.getDefault().getClass()` and
`Files.getFileAttributeView(path, BasicFileAttributeView.class).getClass()` both
return the **abstract/interface** type under CratonVM, vs the concrete
`sun.nio.fs.Windows*` implementation under HotSpot. (`DirProbe`'s other diffs are
just the random temp-dir name — not a bug.)

## Root cause (hypothesis)

CratonVM's NIO provider models the default `FileSystem` and the attribute views
as instances whose runtime class identity is the public abstract type rather than
a platform `sun.nio.fs.Windows*` class. Consistent with SBR-08/SBR-10/SBR-11 —
CratonVM synthesizes JDK objects under their public/abstract names instead of the
internal concrete subclass.

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" FSEq
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" FSEq
```

## Impact

Mostly type-identity; the file operations themselves work. Affects code that
`instanceof`-checks platform NIO types or switches on provider class. Lower
urgency than a functional break.
