# W5-1 — the `System.loadLibrary` allowlist was too wide

Status: fixed (1 in-file change, no out-of-file patches).
Measured on 2026-08-07 on JDK 25.0.3 Windows x64
(`C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`), HotSpot as the oracle.

## The divergence

One character of `CK` output, and `run.sh` compares `CK`/`PASS` lines, not exit
codes — `RJdkJni` exited 0 and still failed the suite:

```
HotSpot :  CK RJdkJni loadedLibrary=net mapped=foo.dll
CratonVM:  CK RJdkJni loadedLibrary=zip mapped=foo.dll
```

`RJdkJni.java:189-202` tries `System.loadLibrary("zip")` first and falls through
to a `net` probe only if `zip` throws `UnsatisfiedLinkError`. HotSpot takes the
fallback. CratonVM did not, because W2-7's `is_vm_provided_jdk_library`
allowlist answered "success" for `zip`.

## The predicted cause was wrong; the prediction was right

Wave 2 predicted the failure correctly but attributed it to static linking —
"`zip` is folded into `libjava` on this image". It is not. `zip.dll` is a real,
separate file in `<java.home>/bin`, and a *cold* `System.loadLibrary("zip")`
loads it fine. The actual rule is dynamic and has nothing to do with linkage:

`jdk.internal.loader.NativeLibraries` refuses to load the same library FILE into
two different class loaders. `java.base` boot-loads `zip.dll` itself —
`Inflater.<clinit>` -> `ZipUtils.loadLibrary()` ->
`BootLoader.loadLibrary("zip")` — so once anything has touched `java.util.zip`,
an app-class-loader `System.loadLibrary("zip")` throws
`UnsatisfiedLinkError: Native Library …\zip.dll already loaded in another classloader`.

Measured, same JVM image, three states:

| state | `loadLibrary("zip")` |
|---|---|
| cold, classpath is a directory | LOADS |
| after any `java.util.zip` native use | THROWS (already loaded in another classloader) |
| cold, but classpath is a `.jar` | THROWS (already loaded in another classloader) |

The rule is per-library-file and symmetric — pre-touching `java.net` makes
`net` *and* `nio` throw; pre-touching `java.util.prefs` makes `prefs` throw.

`RJdkJni` puts itself in the second state deliberately: `main` calls
`zipNatives()` immediately before `libraryLoading()`. That is why the oracle
says `net`, and it is a *test-produced* state rather than a file-layout fact,
so it holds identically on Linux.

## The measurement

`System.loadLibrary(x)` from the app class loader, nothing pre-loaded,
JDK 25.0.3 Windows x64:

```
java             LOADS
zip              LOADS
net              LOADS
nio              LOADS
jimage           LOADS
verify           LOADS
management       LOADS
management_ext   LOADS
instrument       LOADS
extnet           LOADS
prefs            LOADS
j2pkcs11         LOADS
sunec            THROWS UnsatisfiedLinkError: no sunec in java.library.path
sunmscapi        LOADS
jsig             THROWS UnsatisfiedLinkError: no jsig in java.library.path
jvm              THROWS UnsatisfiedLinkError: no jvm in java.library.path
```

Corroborated by the image contents: `<java.home>/bin` has no `sunec.dll`, no
`jsig.dll`, no `jvm.dll` (the last lives in `bin/server`, which is on neither
`java.library.path` nor `sun.boot.library.path`).

## Why the allowlist is the lever at all

Because the real load always fails first. `LoadLibraryW` on the JDK's own DLLs
from a non-JVM process returns `ERROR_MOD_NOT_FOUND` (126) — they are linked
against `jvm.dll`, which CratonVM's process does not have:

```
java, zip, net, nio, jimage, verify, management, management_ext,
instrument, extnet, prefs        LoadLibrary FAILED err=126
j2pkcs11, sunmscapi              LoadLibrary OK   (self-contained)
```

So `load_library_or_throw` reaches the allowlist for every interesting name,
even though `<java.home>\bin` is on this host's `PATH` and therefore on
CratonVM's `java.library.path`.

## Before / after

Removed: `zip`, `sunec`, `jvm` (all platforms) and `jsig` (Windows only).
Kept, now split by platform: `sunmscapi` on Windows, `jsig` elsewhere.

## Known residual — NOT fixed

The dynamic "already loaded in another classloader" rule applies equally to
`net`, `nio` and `prefs`, and a static allowlist cannot model it. There is no
class-loader-scoped `loadedLibraryNames` bookkeeping anywhere in this VM
(`grep` finds none). A program that uses `java.net` and *then* calls
`System.loadLibrary("net")` still gets a silent success here where HotSpot
throws. `net` stays on the list because the oracle needs it: `RJdkJni` never
touches `java.net` before line 195.

Modelling this properly means tracking, per class loader, which JDK native
families have already been bound — a change well outside one file.

## Blast radius

`java.base` does not reach this code for its own bootstrap: `java.util.zip`
loads via `BootLoader.loadLibrary` -> `jdk/internal/loader/NativeLibraries.load`
(registered separately in `native-builtins/src/lib.rs`, which unconditionally
returns success), not via `System.loadLibrary`. Only user-level
`System.loadLibrary`/`System.load`/`Runtime.load0`/`Runtime.loadLibrary0` are
affected. The documented `catch (UnsatisfiedLinkError)` fallbacks in Netty and
Tomcat/tcnative do not name any of the four removed libraries.
