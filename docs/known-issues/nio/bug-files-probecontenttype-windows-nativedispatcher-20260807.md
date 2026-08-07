# Windows: `Files.probeContentType` dies in `WindowsNativeDispatcher.initIDs()`

**Status: OPEN**, found 2026-08-07 on Windows. Not reproducible on Linux.

## Symptom

```
java.lang.UnsatisfiedLinkError: sun/nio/fs/WindowsNativeDispatcher.initIDs()V
        at java.nio.file.Files.probeContentType(Files.java:...)
```

Any `Files.probeContentType(path)` call on Windows. On Linux the same call
answers correctly (`text/plain` for a `.txt` file, measured against HotSpot),
because the Linux detector reads `/etc/mime.types` in pure bytecode; the Windows
one (`sun.nio.fs.RegistryFileTypeDetector`) asks the registry through
`WindowsNativeDispatcher`, whose JNI natives CratonVM does not register.

## How it was found

Not by an application — by the `Files`-surface sweep written while closing the
retired `bug-h2-windows-files-setattribute-abstract` write-up (the
`Files.setAttribute` / abstract-`FileSystemProvider` gap, fixed 2026-08-07).
It is **not** a residual of that bug: the mechanism is different
(`UnsatisfiedLinkError` on a missing JNI native, not `AbstractMethodError` on an
abstract declaration with no implementation), and it lives in a different
subsystem (the Windows registry MIME lookup, not the file-attribute surface).
It is filed separately so it is not lost.

## Reproducing (seconds)

```java
Path f = Files.createTempFile("probe", ".txt");
Files.write(f, "hello".getBytes());
System.out.println(Files.probeContentType(f));   // HotSpot-on-Windows: text/plain
```

```
cratonvm.exe --java-home <jdk25> -c <classes> ProbeContentType
```

## Next step

Two candidate shapes, in order of preference:

1. Register the `sun.nio.fs.WindowsNativeDispatcher` registry entry points the
   detector actually calls (`initIDs`, and the `RegOpenKeyEx`/`RegQueryValueEx`
   pair behind `RegistryFileTypeDetector.implProbeContentType`) so the real JDK
   bytecode runs and the answer comes from the same registry HotSpot reads.
2. Failing that, a `Files.probeContentType` native. Note the JDK contract
   explicitly permits `null` ("the content type, or null if the content type
   cannot be determined"), so a detector that returns `null` for an unknown
   extension is legal — but an invented extension→MIME table is a compatibility
   substitution, and should be labelled as one if it is taken.

Do **not** register `initIDs` as a bare no-op: the registry lookups behind it
are the part that has to work, and a silent no-op would turn a loud
`UnsatisfiedLinkError` into a wrong answer.
