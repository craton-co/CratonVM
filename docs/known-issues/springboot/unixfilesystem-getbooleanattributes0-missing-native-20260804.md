# `java/io/UnixFileSystem.getBooleanAttributes0` / `WinNTFileSystem.getBooleanAttributes0` never registered — `File.exists()` throws `UnsatisfiedLinkError` once real `File` bytecode runs

**Status: OPEN — found 2026-08-04**

## Symptom

`DiskSpaceHealthIndicatorTests.whenPathDoesNotExistDiskSpaceIsDown()` fails:

```
java.lang.UnsatisfiedLinkError: java/io/UnixFileSystem.getBooleanAttributes0(Ljava/io/File;)I
   java.io.UnixFileSystem.hasBooleanAttributes(UnixFileSystem.java:180)
   java.io.File.exists(File.java:790)
   org.springframework.boot.health.application.DiskSpaceHealthIndicator.doHealthCheck(DiskSpaceHealthIndicator.java:75)
   org.springframework.boot.health.contributor.AbstractHealthIndicator.health(AbstractHealthIndicator.java:80)
   org.springframework.boot.health.application.DiskSpaceHealthIndicatorTests.whenPathDoesNotExistDiskSpaceIsDown(...)
```

`.err.log` confirms this is a real, not-yet-implemented native gap (not a
crash or timeout):

```
WARN cratonvm_vm::vm::vm_exec: Missing native method in real-JDK mode method=java/io/UnixFileSystem.getBooleanAttributes0(Ljava/io/File;)I
```
(logged twice, once per attempt, then `System.exit(1)`.)

This is **not** the same bug as the already-FIXED
`spring-boot-health-rerun-20260717-FIXED.md` doc for this exact test method:
that bug was an `AssertionFailedError` (`expected: DOWN, but was: UP`) caused
by hardcoded `getUsableSpace()`/`getSpace()` stubs always returning a huge
constant. That fix (real `statvfs`/`GetDiskFreeSpaceExW`-backed disk-space
query, `native-builtins/src/phases_late.rs`) is unrelated to and does not
touch this native — today's failure is a completely different exception type
(`UnsatisfiedLinkError`, thrown before disk space is even computed) on the
same test method, so it does not disprove that the disk-space fix still
holds; it is a separate, previously-undiscovered gap on the same code path.

## Root cause

`DiskSpaceHealthIndicator.doHealthCheck` (Spring Boot source,
`module/spring-boot-health/.../DiskSpaceHealthIndicator.java`) ends with:

```java
builder.withDetail("total", this.path.getTotalSpace())
    .withDetail("free", diskFreeInBytes)
    .withDetail("threshold", this.threshold.toBytes())
    .withDetail("path", this.path.getAbsolutePath())
    .withDetail("exists", this.path.exists());   // line 75
```

`DiskSpaceHealthIndicatorTests` declares `@Mock private File fileMock;`
under `@ExtendWith(MockitoExtension.class)`. Per the mechanism already
documented in `spring-boot-health-rerun-20260717-FIXED.md`'s "Root cause 2":
Mockito's inline mock maker retransforms the **entire** `java.io.File` class
via `Instrumentation.redefineClasses` the moment any test in the class needs
a mock (`@BeforeEach` runs for every test method) — process-wide, not scoped
to the mocked field. Once `java.io.File` is redefined, CratonVM's
`check_override` forced-native fast path for `File.exists()` no longer
applies (that path only intercepts the *original*, non-redefined class), so
the real (redefined) bytecode runs for every `File` instance, including the
genuinely-real `new File("does/not/exist")` under test.

Real bytecode `File.exists()` delegates to `FS.hasBooleanAttributes(this,
BA_EXISTS)` → `UnixFileSystem.hasBooleanAttributes` (or the `WinNTFileSystem`
equivalent), which calls the raw native `getBooleanAttributes0(File)`.
CratonVM's `native-builtins/src/phases_late/nio_file.rs` (the `for fs_cls in
&["java/io/WinNTFileSystem", "java/io/UnixFileSystem"]` registration loop,
~line 6572) registers `getBooleanAttributes` (no trailing `0`,
`"(Ljava/io/File;)I"`, returning the full BA_EXISTS/BA_REGULAR/BA_DIRECTORY/
BA_HIDDEN bitmask via `std::fs::metadata`) but never registers
`getBooleanAttributes0` — the raw JNI-native counterpart that newer-JDK
`FileSystem.hasBooleanAttributes`/`getBooleanAttributes` wrapper methods
call directly. Since `File.FS` (populated by `native_file_clinit`, see the
already-fixed `file-fs-native-clinit-never-set-FIXED.md`) is a real,
non-null `UnixFileSystem`/`WinNTFileSystem` instance now that that other bug
is fixed, real bytecode successfully reaches `FS.hasBooleanAttributes(...)`
— but the native call underneath it has simply never been implemented,
producing `UnsatisfiedLinkError` instead of a wrong answer.

## Fix (not applied — flagging only, per task scope)

Register `getBooleanAttributes0` on both `java/io/WinNTFileSystem` and
`java/io/UnixFileSystem` (same loop, `native-builtins/src/phases_late/nio_file.rs`
~line 6572-6610), reusing the same `std::fs::metadata`-based bitmask logic
already implemented for `getBooleanAttributes` — the two methods are
expected to return byte-identical bitmasks on real JDK (`getBooleanAttributes`
is the non-native public wrapper that ORs in `BA_HIDDEN` on top of
`getBooleanAttributes0`'s result on Unix, so `getBooleanAttributes0` alone
should return `BA_EXISTS|BA_REGULAR|BA_DIRECTORY` without the hidden-file
check).

## Affected classes

- `module/spring-boot-health` — `org.springframework.boot.health.application.DiskSpaceHealthIndicatorTests`

(Likely also affects any other test class combining a real `java.io.File`
instance with an `@Mock File` field in the same JUnit class — the same
"Mockito redefinition forces real bytecode VM-wide" mechanism the prior
fixed doc already flagged as general, not narrow to this one test.)
