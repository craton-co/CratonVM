# `Files.readAttributes(Path, PosixFileAttributes.class, ...)` ClassCastException on Windows

**Status: OPEN — found 2026-07-23**

## Symptom

```
java.lang.ClassCastException: sun.nio.fs.WindowsFileAttributes cannot be cast to java.nio.file.attribute.PosixFileAttributes
	at java.nio.file.Files.getPosixFilePermissions(Files.java:1952)
	at org.springframework.boot.system.ApplicationPid.canWritePosixFile(ApplicationPid.java:135)
	at org.springframework.boot.system.ApplicationPid.assertCanOverwrite(ApplicationPid.java:128)
	at org.springframework.boot.system.ApplicationPid.write(ApplicationPid.java:114)
	at org.springframework.boot.context.ApplicationPidFileWriter.writePidFile(ApplicationPidFileWriter.java:158)
```

`ApplicationPidTests.overwriteExistingPid()` fails outright with this
`ClassCastException`. `ApplicationPidFileWriterTests.tryEnvironmentPreparedEvent()`
/ `tryReadyEvent()` don't see the exception directly (it's caught one
frame up, inside `ApplicationPid.assertCanOverwrite`'s caller path via a
different call site — the pid-file write silently fails and logs a `WARN`)
but consequently their captured-output assertions ("Expecting actual not
to be empty") fail because the expected PID-file-write log line never
appears.

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard6/logs/core_spring-boot.org.springframework.boot.system.ApplicationPidTests.out.log`
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard6/logs/core_spring-boot.org.springframework.boot.context.ApplicationPidFileWriterTests.out.log`

## Root cause

Confirmed at file:line. `ApplicationPid.canWritePosixFile` (Spring Boot's own,
real bytecode) is written to *rely on* `UnsupportedOperationException` as the
signal that the current filesystem doesn't support POSIX permissions:

```java
// apps/spring-boot/core/spring-boot/src/main/java/org/springframework/boot/system/ApplicationPid.java:133-147
private boolean canWritePosixFile(Path file) throws IOException {
    try {
        Set<PosixFilePermission> permissions = Files.getPosixFilePermissions(file, LinkOption.NOFOLLOW_LINKS);
        ...
    }
    catch (UnsupportedOperationException ex) {
        // Assume that we can
        return true;
    }
}
```

On real HotSpot/Windows, `WindowsFileSystemProvider.readAttributes(Path,
Class<A> type, ...)` checks the requested attribute-view `type` and throws
`UnsupportedOperationException` when asked for `PosixFileAttributes.class`
(Windows has no POSIX view), which this `catch` block is written to expect.

CratonVM's native override of the generic
`Files.readAttributes(Path, Class, LinkOption[])` overload —
`p59_files_read_attributes` in `native-builtins/src/phases_late.rs:27794`
(registered at `phases_late.rs:27452`) — **never inspects the requested
`type` `Class` argument at all**. It always calls
`basic_file_attributes_alloc` (`phases_late.rs:27637-27652`), which on
Windows unconditionally allocates a `sun/nio/fs/WindowsFileAttributes`
object regardless of whether the caller asked for `BasicFileAttributes`,
`DosFileAttributes`, or `PosixFileAttributes`. Since
`Files.getPosixFilePermissions` is generic (`<A extends BasicFileAttributes>
A readAttributes(...)`), javac inserts a `checkcast PosixFileAttributes` at
the call site around the return value; that cast then fails against the
always-`WindowsFileAttributes` object CratonVM hands back — producing
exactly the observed `ClassCastException` instead of real HotSpot's
`UnsupportedOperationException`.

**Fix direction (not applied — investigation only):** `p59_files_read_attributes`
needs to read `args[1]` (the requested `Class` object's name) and, on
Windows, throw `UnsupportedOperationException` for `PosixFileAttributes`
(and any other view type it doesn't model) instead of silently returning a
`WindowsFileAttributes` object that can't satisfy the requested checkcast.

## Affected classes

| Module | Class |
|---|---|
| core/spring-boot | org.springframework.boot.system.ApplicationPidTests |
| core/spring-boot | org.springframework.boot.context.ApplicationPidFileWriterTests |
