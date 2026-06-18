# Gap: `BasicFileAttributes.isDirectory()Z` has no Code attribute

**Discovered:** 2026-06-09 (cross-VM comparison run)  
**Severity:** High — blocks JUnit Platform classpath scanner from walking any file tree, which means `--select-package` discovery never runs. Affects Commons Math test runner and any app that uses `Files.walkFileTree`.  
**Status:** **Fixed 2026-06-09** — `register_p59_file_attributes` promoted to `register_essential_natives`, plus a new `BasicFileAttributes.fileKey()` native. See "Resolution" below.  

---

## Symptom

```
java.lang.AbstractMethodError: method java/nio/file/attribute/BasicFileAttributes.isDirectory()Z has no Code attribute
    at java.nio.file.Files.walkFileTree(Files.java:2536)
    at java.nio.file.FileTreeWalker.walk(FileTreeWalker.java:306)
    at java.nio.file.FileTreeWalker.visit(FileTreeWalker.java:275)
    at java.nio.file.FileTreeWalker.doVisit(FileTreeWalker.java:240)
    at java.nio.file.FileTreeWalker.next(FileTreeWalker.java:372)
    at java.nio.file.Files.walkFileTree(Files.java:2536)
    at org.junit.platform.commons.util.ClasspathScanner.findClassesForPath(ClasspathScanner.java:125)
    at org.junit.platform.commons.util.ClasspathScanner.findClassesForUri(ClasspathScanner.java:110)
    at org.junit.platform.commons.util.ClasspathScanner.findClassesForUris(ClasspathScanner.java:100)
    at org.junit.platform.engine.support.discovery.EngineDiscoveryRequestResolution.resolve(...)
```

Triggered by:
```
java -cp <junit-standalone>;<cm-classes> \
  org.junit.platform.console.ConsoleLauncher execute \
  --select-package org.apache.commons.math4.transform ...
```

The JUnit launcher wraps the error in:
```
org.junit.platform.commons.JUnitException: PackageSelector [packageName = 'org.apache.commons.math4.transform'] resolution failed
  Caused by: java.lang.AbstractMethodError: method java/nio/file/attribute/BasicFileAttributes.isDirectory()Z has no Code attribute
```

---

## Root cause analysis

`BasicFileAttributes` is a `java.nio.file.attribute` **interface** declared in `module java.base`. Its methods — `isDirectory()`, `isRegularFile()`, `size()`, `lastModifiedTime()`, etc. — are all abstract (no default implementations).

The real JDK concrete implementation on Windows is `sun.nio.fs.WindowsFileAttributes` (package-private, returned by the native `WindowsFileSystem` provider). CratonVM must intercept the `Files.walkFileTree` → `FileTreeWalker` → `FileAttributes.isDirectory()` dispatch chain.

The `AbstractMethodError: has no Code attribute` is CratonVM's specific error form when the interpreter finds a method registered in the vtable (so dispatch succeeds) but the method body has been registered as a native/synthetic stub **without** a `Code` attribute — the interpreter requires a `Code` attribute to execute bytecode, and since the stub has none and there's no native handler registered, it throws.

**Likely location:** `native-builtins/src/nio_file.rs` or equivalent. The file-attribute type returned by `FileTreeWalker` during a walk is a `WindowsFileAttributes` (or CratonVM synthetic equivalent). The method `isDirectory()` on that object resolves to a stub that has no bytecode and no native dispatch entry.

**Callchain from the JDK source (`Files.walkFileTree` line 2536):**
```
FileTreeWalker.visit(path, attrs) {
    BasicFileAttributes attrs = ...;   // result from Files.readAttributes
    boolean isDir = attrs.isDirectory(); // <-- throws here
    ...
}
```

The `attrs` object at the crash point is the `BasicFileAttributes` view returned by `WindowsFileAttributeViews` (or CratonVM's synthetic equivalent). The `isDirectory()` call goes through interface dispatch to the concrete `WindowsFileAttributes.isDirectory()` which has no Code attribute in CratonVM's class representation.

---

## Reproduction

```bash
CV="C:/craton/CratonVM/target/release/cratonvm.exe"
JDK="C:/Program Files/Java/jdk-25"
JUNIT="C:/craton/CratonVM/.bench-cache/junit-platform-console-standalone-1.10.2.jar"
CM="C:/craton/CratonVM/apps/_test-suites/commons-math"
M2="C:/Users/Victor/.m2/repository"
CP="$JUNIT;$CM/commons-math-transform/target/classes;$CM/commons-math-transform/target/test-classes"
CP="$CP;$CM/commons-math-core/target/classes"
CP="$CP;$M2/org/apache/commons/commons-numbers-rng/1.2/commons-numbers-rng-1.2.jar"
CP="$CP;$M2/org/apache/commons/commons-numbers-core/1.2/commons-numbers-core-1.2.jar"

"$CV" --java-home "$JDK" --Xmx 2g -cp "$CP" \
  org.junit.platform.console.ConsoleLauncher execute \
  --select-package org.apache.commons.math4.transform \
  --details=summary --disable-banner
```

The error fires ~11 seconds in (after JUnit5 engine init). HotSpot and TornadoVM are unaffected.

---

## Minimal repro (self-contained)

```java
// FileWalkTest.java
import java.nio.file.*;
import java.nio.file.attribute.*;

public class FileWalkTest {
    public static void main(String[] args) throws Exception {
        Path dir = Paths.get(System.getProperty("java.home"));
        Files.walkFileTree(dir, new SimpleFileVisitor<>() {
            @Override
            public FileVisitResult visitFile(Path f, BasicFileAttributes attrs) {
                System.out.println(f + " dir=" + attrs.isDirectory());
                return FileVisitResult.TERMINATE;
            }
        });
    }
}
```

Compile with HotSpot JDK 25, run under CratonVM — should reproduce AbstractMethodError immediately.

---

## Fix direction

1. **Find the concrete `BasicFileAttributes` implementation** CratonVM returns from `Files.readAttributes` / `FileTreeWalker`. On Windows this is `sun.nio.fs.WindowsFileAttributes`. Check `native-builtins/src/nio_file.rs` or `native-io/`.

2. **Register a native handler for `isDirectory()`** on that concrete class, or ensure real JDK bytecode is used instead of a stub. All methods of `BasicFileAttributes` are likely missing: `isDirectory`, `isRegularFile`, `isSymbolicLink`, `isOther`, `size`, `lastModifiedTime`, `lastAccessTime`, `creationTime`, `fileKey`.

3. **Verify with `FileWalkTest`** above. Once `isDirectory()` works, the JUnit5 `ClasspathScanner` will be able to enumerate classes and Commons Math tests will begin executing.

4. **Do not add a synthetic stub** that hard-codes a return value. The implementation must call the real OS stat (via the existing `sun/nio/ch/Net` or `WindowsFileSystem` native path) or route to the real JDK `WindowsFileAttributes` bytecode.

---

## Impact scope

Any code that calls `Files.walkFileTree`, `Files.walk`, or `Files.find` will fail with this error. This includes:
- JUnit Platform `ClasspathScanner` (test discovery)  
- Any framework that scans classpath directories (Spring component scan, Hibernate entity scanning, etc.)
- `Files.copy` on directories (uses walkFileTree internally)

---

## Resolution (2026-06-09)

**Root cause.** `register_p59_file_attributes` (which registers `Files.readAttributes(Path, Class, LinkOption[])` and the `BasicFileAttributes.{isDirectory,isRegularFile,size,creationTime,lastAccessTime,lastModifiedTime}` natives backed by `std::fs::metadata`) was only wired into `register_synthetic_overrides`, which is `#[cfg(feature = "synthetic-jdk")]` — compiled out in real-JDK CLI builds. So in real-JDK mode the dispatch of `attrs.isDirectory()` had no native to fall through to.

Confirmed with `CRATONVM_DBG_NOCODE=1`: the receiver's runtime class was the `BasicFileAttributes` interface itself (`ClassId(398)`), so the interface-method rescue in `interpreter::execute` looked for a native on `java/nio/file/attribute/BasicFileAttributes` and found none → `AbstractMethodError`.

**Fix.** Two changes, both in `native-builtins`:

1. `register_essential_natives` (in `lib.rs`, end of function) now calls `crate::phases_late::register_p59_file_attributes(registry)`. Same pattern as the existing `register_p66_break_iterator` promotion. The synthetic returns a 5-field BFA populated from real `std::fs::metadata` — no fabricated values, no synthetic stub: directory / regular-file / size all reflect the actual on-disk state.
2. Added `BasicFileAttributes.fileKey()Ljava/lang/Object;` native returning `null`. This matches the documented JDK contract for filesystems that don't expose unique file keys (Windows FAT-class / network shares), and lets `FileTreeWalker.wouldLoop` (the only `fileKey` consumer in the JDK walker) skip its identity comparison instead of throwing `AbstractMethodError`.

**Verification.**

```text
$ cratonvm  FileWalkTest         # minimal repro
walking C:/Program Files/Java/jdk-25
C:/Program Files/Java/jdk-25/bin/api-ms-win-core-console-l1-1-0.dll dir=false
OK

$ cratonvm  FileWalkDeep sample  # 4-dir, 4-file mock tree, sums sizes
dirs=4 files=4 bytes=26

$ java       FileWalkDeep sample  # HotSpot for parity
dirs=4 files=4 bytes=26

$ cratonvm  ConsoleLauncher execute --select-package …commons-math…
# AbstractMethodError on BasicFileAttributes.isDirectory()Z is gone;
# `ClasspathScanner.findClassesForPath` proceeds. Remaining failures are
# unrelated downstream gaps (NoClassDefFoundError for the test class itself
# plus the AnonymousObject$6.getInputStream() gap tracked separately).
```

The synthetic-jdk `basic_file_attributes_p59` unit test in `vm.rs` continues to exercise the same registration path under that feature.
