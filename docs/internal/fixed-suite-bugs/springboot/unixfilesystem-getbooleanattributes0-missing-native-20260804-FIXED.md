# `java/io/UnixFileSystem.getBooleanAttributes0` / `WinNTFileSystem.getBooleanAttributes0` never registered — `File.exists()` throws `UnsatisfiedLinkError` once real `File` bytecode runs

**Status: FIXED — 2026-08-04.** Retired from `docs/known-issues/springboot/`.
Branch `fix/unixfs-getbooleanattributes0-20260804`, commits `852edeaf8`,
`72ebdc61f`, `8e0197d60`, `5b603e94d`.

The reported native was one of **fifteen** in the same class. The original
report is preserved verbatim below the fix write-up.

## What the report got right, and what it missed

Right: the native is genuinely unregistered, the mechanism that exposes it is
Mockito's process-wide `java.io.File` redefinition, and the fix is a
`std::fs::metadata`-backed bitmask.

Missed: **`getBooleanAttributes0` is not a one-off gap.** JDK 22 moved almost
every `java.io.FileSystem` JNI native behind a `0`-suffixed private one with a
plain-Java public wrapper in front of it. CratonVM registered only the pre-22
spelling. Those wrappers are *concrete bytecode*, and neither
`java/io/UnixFileSystem` nor `java/io/WinNTFileSystem` appears in the
`check_override` allow-list in `vm_exec.rs::invoke_on_class_shared_inner` — so
on a real JDK 22+ **every one of those bare registrations is inert**: the
wrapper's bytecode runs and asks for the `0` native, which was never
registered.

The report's own fix suggestion ("reusing the same `std::fs::metadata`-based
bitmask logic already implemented for `getBooleanAttributes`") would have
closed one line of a fifteen-line hole, and would have got the `BA_HIDDEN`
half of it backwards on one of the two platforms (see below).

## The full gap, as measured

`probes/FsNativeSurfaceProbe.java` drives all sixteen `FileSystem` operations
through the real `FileSystem` bytecode — reflectively, via `java.io.File.FS`,
so CratonVM's own `java/io/File` overrides cannot answer for them — and prints
one `KEY=value` line per operation. Run under HotSpot and under CratonVM, the
two outputs diff literally.

**Before** (`cratonvm-bafs0-base`, dev `c7c63d8818`, JDK 25 real-JDK mode,
Linux): `PROBE_FAILURES=11`, plus five lines that returned a wrong answer
rather than throwing.

| Native | Before | Cause |
|---|---|---|
| `getBooleanAttributes0(File)I` | `UnsatisfiedLinkError` | never registered — **the reported bug** |
| `createFileExclusively0(String)Z` | `UnsatisfiedLinkError` | never registered under *either* spelling |
| `setPermission0(File,I,Z,Z)Z` | `UnsatisfiedLinkError` | never registered under *either* spelling |
| `getNameMax0(String)J` | `UnsatisfiedLinkError` | registered as `(Ljava/lang/String;)I`. The return type is `long` on `UnixFileSystem` and `int` on `WinNTFileSystem`, and a native is keyed by descriptor as well as name, so the Unix one never resolved |
| `checkAccess0` / `checkAccess` | wrong answer | answered "does the path exist?" for every mode, so `File.canWrite()` returned `true` for a file `File.setReadOnly()` had just locked down (`RO_write_after=true`, HotSpot `false`) |
| `getBooleanAttributes` | wrong answer | never set `BA_HIDDEN` (`BA_hidden=3`, HotSpot `11`) |
| `list` / `list0` | wrong answer | returned an empty array where the JDK returns `null`, so `File.list()` on a plain file looked like an empty directory |
| `list` / `File.list` / `File.listFiles` / `File.listRoots` | wrong type | allocated with `new_array(ArrayElementType::Reference, ..)` — an untyped `Object[]`, where every one of those descriptors declares `[Ljava/lang/String;` or `[Ljava/io/File;` and every caller assigns to a typed local (a checkcast) |
| `getFinalPath0` | wrong answer, Windows | returned `std::fs::canonicalize` verbatim, i.e. with the `\\?\` extended-length prefix. Since `WinNTFileSystem.canonicalize(s)` is `getFinalPath(canonicalize0(s))`, `canonicalize` answered `\\?\C:\…\x.txt` where HotSpot answers `C:\…\x.txt`, breaking any `startsWith(canonicalBase)` containment check |
| `getLastModifiedTime0`, `getLength0`, `list0`, `createDirectory0`, `setLastModifiedTime0`, `setReadOnly0`, `getSpace0` | unreachable | registered only under the pre-22 bare name |
| `WinNTFileSystem.delete0(File,Z)Z` | unreachable | registered with the Unix 1-argument descriptor; Windows passes a second `allowDeleteReadOnlyFiles` |
| `listRoots0()I`, `getDriveDirectory(I)`, `initIDs()V` | unreachable | never registered |

Two of those did not surface in the probe run only because they are Windows-only
(`delete0/2`, `listRoots0`) — they are the same defect and are fixed with it.

`BA_HIDDEN` is the one place where the two classes genuinely disagree and a
shared body would be wrong: `UnixFileSystem.getBooleanAttributes0` **never**
sets it (the public wrapper ORs in `isHidden(f)`, a leading-`.` test on the
*name* that does not consult the filesystem, so a non-existent `.foo` reports
`BA_HIDDEN` and nothing else), while `WinNTFileSystem.getBooleanAttributes0`
sets it straight from `FILE_ATTRIBUTE_HIDDEN` and its wrapper adds nothing.

## Fix

`native-builtins/src/phases_late/nio_file.rs`. Every operation is now
registered under **both** spellings — the `0` one for JDK 22+, the bare one for
pre-22 JDKs and CratonVM's synthetic `java.io.FileSystem` — with the three
per-class differences (`getNameMax0`'s return type, `delete0`'s arity,
`getBooleanAttributes0`'s hidden bit) handled explicitly rather than shared.
The shared bodies live in `fs_boolean_attributes0` / `fs_boolean_attributes` /
`fs_check_access` / `fs_set_permission` / `fs_list_dir` /
`fs_list_roots_bitmask`, with a banner explaining why the bare spelling alone
registers a method nothing calls.

`checkAccess` now uses `access(2)` on Unix — the same call the JDK's native
makes, so it honours ACLs and read-only mounts a permission-bit test would miss
— and the READONLY attribute on Windows.

## Verification

**Probe, Linux** (`/data/data/jdk25-real`, Azure): CratonVM output is
**byte-identical to the HotSpot control**, 48 lines, `PROBE_FAILURES=0`
(was 11).

**Probe, Windows** (Temurin 25.0.3): identical to the Windows HotSpot control,
`PROBE_FAILURES=0`. `getFinalPath0` was the single remaining differing line
before `5b603e94d`; it is the reason that commit exists.

**The reported test.** `run-sb7.sh module/spring-boot-health
org.springframework.boot.health.application.DiskSpaceHealthIndicatorTests`:

| | tests | failed |
|---|---|---|
| baseline `cratonvm-bafs0-base` | 3 | **1** (`UnsatisfiedLinkError: java/io/UnixFileSystem.getBooleanAttributes0`) |
| fixed `cratonvm-bafs0-fix` | 3 | **0** |

**Regression sweep.** 18 File-heavy Spring Boot test classes across
`spring-boot-health`, `core/spring-boot`, `spring-boot-devtools`,
`spring-boot-web-server`, `spring-boot-loader`, `spring-boot-loader-tools` and
`spring-boot-jarmode-tools`, run under both binaries and diffed row by row.
**Exactly one row changed** — `DiskSpaceHealthIndicatorTests` `failed=1` →
`failed=0`. Everything else, including the five pre-existing `LOADFAIL`s in
`spring-boot-loader-tools` / `jarmode-tools` / `devtools` and the two
pre-existing `NestedJarFileTests` failures, is identical in both arms. Zero
`Missing native method in real-JDK mode` warnings remain anywhere in the fixed
arm's logs (the baseline arm logged `getBooleanAttributes0` twice per affected
run).

`cargo test -p cratonvm-native-builtins --lib`: 3249 passed, 0 failed.

## Measurement trap worth knowing

`--dump-missing-natives FILE` (and `-XX:AuditMissingNatives`) **turn the
failure green**. Audit mode makes the missing-native path return a typed
default instead of throwing, so the first baseline run of
`DiskSpaceHealthIndicatorTests` under that flag reported `tests=3 failed=0`
while still logging the `Missing native method` warning twice. The flag also
wrote no dump file, because `SbRunner` ends with `System.exit`. A baseline
taken with it is worthless for this class of bug — run without it.

---

# Original report (2026-08-04, preserved verbatim)

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
