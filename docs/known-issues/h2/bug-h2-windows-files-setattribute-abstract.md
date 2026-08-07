# Windows: `Files.setAttribute` dispatches to the ABSTRACT `FileSystemProvider.setAttribute`

**Status: OPEN**, found 2026-08-07 on Windows. Not reproducible on Linux.
Blocks every filesystem `org.h2.test.unit.TestFileSystem` exercises, at the
fourth sub-test of each.

## Symptom

```
java.lang.AbstractMethodError: method java/nio/file/spi/FileSystemProvider.setAttribute(
    Ljava/nio/file/Path;Ljava/lang/String;Ljava/lang/Object;[Ljava/nio/file/LinkOption;)V
    has no Code attribute
        at java.nio.file.Files.setAttribute(Files.java:1764)
        at org.h2.store.fs.disk.FilePathDisk.setReadOnly(FilePathDisk.java:293)
        at org.h2.store.fs.FileUtils.setReadOnly(FileUtils.java:330)
        at org.h2.test.unit.TestFileSystem.testSetReadOnly(TestFileSystem.java:399)
        at org.h2.test.unit.TestFileSystem.testFileSystem(TestFileSystem.java:377)
```

`Files.setAttribute(path, "dos:readonly", …)` calls
`provider(path).setAttribute(...)`. `java.nio.file.spi.FileSystemProvider`
declares that method `public abstract`; the receiver at runtime is
`sun.nio.fs.WindowsFileSystemProvider`, which overrides it. The VM resolved and
invoked the abstract declaration instead of the override — the message is the
VM's own ("has no Code attribute"), so this is a virtual-dispatch/override
resolution defect, not a missing JDK class.

## Scope

* **Windows only.** The same probe, the same H2 checkout content and the same
  branch pass this sub-test on Linux — `testFileSystem` there clears
  `testSetReadOnly` and goes on to complete every prefix.
* Every prefix fails identically, because `testSetReadOnly` runs early in
  `testFileSystem(String)`. Plain disk, `nioMapped:` and `split:nioMapped:` were
  all measured: 3/3 the same `AbstractMethodError`, at 9.7 s / 3.0 s / 4.6 s.
* It is what `TestFileSystem` fails with on Windows *today*, which also means
  the class cannot currently reach any of the mapped-buffer behaviour the
  retired `bug-h2-niomapped-unmap-gc-timeout` write-up is about.

## Reproducing (seconds)

```
H2=<checkout>/apps/h2database/h2
javac -cp "$H2/target/classes;$H2/target/test-classes" -d /tmp/tfs \
  apps/h2database-suite-runner/probes/TfsProbe.java
cratonvm.exe --java-home <jdk25> --Xmx 1g \
  -c "$H2/target/classes;$H2/target/test-classes;/tmp/tfs" \
  TfsProbe @BASE@/fs
```

Run it from a scratch directory — H2's `BASE_TEST_DIR` is `./data`, relative to
the working directory.

## Next step

A pure-JDK witness first, with no H2 in it: call
`Files.setAttribute(Path.of("x"), "dos:readonly", Boolean.TRUE)` on a real file
and compare against HotSpot. If that reproduces, the question is why the
receiver's `WindowsFileSystemProvider.setAttribute` override does not win the
`invokevirtual` on a `FileSystemProvider`-typed receiver — a
`sun.nio.fs`-package override that only exists on Windows is the obvious thing
the vtable/override machinery may not be seeing.
