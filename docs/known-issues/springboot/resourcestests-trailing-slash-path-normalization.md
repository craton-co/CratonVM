# ResourcesTests: trailing-separator path normalization not applied to `java.nio.file.Path`

**Status: OPEN.** Originally filed 2026-07-18 as "RESOLVED", but every
re-run since (2026-07-23, 2026-07-28, and now 2026-08-04 — see below) shows
the same test still failing; the header was never corrected to match. Moved
from `fixed-suite-bugs/` to `docs/known-issues/` on 2026-08-04
to stop it being read as closed. The title has also been broadened: the
2026-08-04 rerun reproduces the identical failure on **Linux**, confirming
the bug is in CratonVM's `java.nio.file.Path`/`Files` layer generally, not a
Windows-only path-separator quirk.

> **Confirmed still failing 2026-07-23** — `ResourcesTests.whenAddDirectoryAndResourceAlreadyExistsThenIllegalStateExceptionIsThrown`
> reproduces the *exact* original symptom in the `craton-rerun-20260723`
> results (`ERROR_DIRECTORY`/os error 267, same test, same line):
> `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard3/logs/test-support_spring-boot-test-support.org.springframework.boot.testsupport.classpath.resources-c869c3f8368d.out.log`.
>
> **The stated root cause/fix below is incomplete, not wrong in principle —
> it just never covered the code path this test exercises.** The only commit
> that ever touched this (`8c7f397b0`, still present in this worktree and on
> `origin/dev`'s tip) wired the trailing-separator trim
> (`p57_trim_file_trailing_separator`) into exactly two places:
> `java.io.File.getAbsolutePath()` and `getAbsoluteFile()`
> (`native-builtins/src/phases_late.rs:16490-16527`). `Resources.addResource`
> never touches `java.io.File` — it calls `this.root.resolve(name)` on a
> `java.nio.file.Path` and then `Files.writeString(resourcePath, ...)`, pure
> NIO2 API. That code path has no call to the trim function anywhere (only 2
> call sites exist in the whole crate, both listed above), so the trailing
> `/` an ordinary `Path.resolve()` should strip was never actually fixed for
> `Path`/`Files` — only for the legacy `File` API. The doc's own "Validation"
> section claim ("`ResourcesTests`: passed with JIT and `--nojit`") could not
> have exercised this test method through the actual fixed code, since the
> fix doesn't sit on this test's call path at all; that verification was
> either against a different build state or mistaken.
>
> **Confirming next step:** trace whichever native code actually backs
> `sun.nio.fs.WindowsPath`'s string construction / `resolve()` (if
> real-bytecode `java.nio.file.Path` is in play, the bug is likely upstream
> of any native hook — in whatever CratonVM does to seed the synthetic
> `Path`'s raw string, or in the native file-write syscall wrapper behind
> `Files.writeString` reading the object's string form without normalizing
> it), then apply the same "strip except for roots" logic there.
>
> **Confirmed still failing 2026-07-28 (craton-rerun-20260728)** — identical
> symptom to the 2026-07-23 note above: same test
> (`whenAddDirectoryAndResourceAlreadyExistsThenIllegalStateExceptionIsThrown`),
> same `os error 267`/`ERROR_DIRECTORY`, same failing line
> (`Resources.addResource(Resources.java:114)`):
> ```
> => java.lang.IllegalStateException: IOException: Неверно задано имя папки. (os error 267)
>    org.springframework.boot.testsupport.classpath.resources.Resources.addResource(Resources.java:114)
> ```
> Log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard1/logs/test-support_spring-boot-test-support.org.springframework.boot.testsupport.classpath.resour-c869c3f8368d.out.log`.
> Not re-investigated further this session (log-analysis/triage only, no
> build or test execution performed) — the "confirming next step" above
> (tracing `Path`/`Files` construction rather than `File`) remains
> un-attempted.

## Confirmed still failing 2026-08-04 (Linux — cross-platform confirmation)

Residual rerun `craton-residual32-20260804-s4`, same class, same method,
same setup call and failing line as every prior recurrence
(`Resources.addResource(Resources.java:114)`), but this time on a **Linux**
host, with the platform-appropriate errno instead of Windows'
`ERROR_DIRECTORY`/267:

```
=> java.lang.IllegalStateException: IOException: Is a directory (os error 21)
   org.springframework.boot.testsupport.classpath.resources.Resources.addResource(Resources.java:114)
   org.springframework.boot.testsupport.classpath.resources.ResourcesTests.whenAddDirectoryAndResourceAlreadyExistsThenIllegalStateExceptionIsThrown(ResourcesTests.java:146)
```

Log:
`apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s4/all-jit/logs/test-support_spring-boot-test-support.org.springframework.boot.testsupport.classpath.resources.ResourcesTests.{out,err}.log`

Test source (`ResourcesTests.java:145-147`, matches every prior recurrence):

```java
void whenAddDirectoryAndResourceAlreadyExistsThenIllegalStateExceptionIsThrown() {
    this.resources.addResource("one/two/three/", "content", true);
    assertThatIllegalStateException().isThrownBy(() -> this.resources.addDirectory("one/two/three"));
}
```

`Resources.addResource` (`Resources.java:103-118`) does
`Path resourcePath = this.root.resolve(name)` (name still carries the
trailing `/`) then `Files.writeString(resourcePath, ...)`. On Linux this
throws `EISDIR` ("Is a directory", errno 21) for the same underlying reason
the Windows recurrences hit `ERROR_DIRECTORY` (267): CratonVM's synthetic
`Path`/NIO layer keeps the literal trailing separator on an ordinary
non-root path, so the write bridge receives a directory-shaped path instead
of a file path — this is OS-errno-portable evidence for the exact root
cause already suspected in the 2026-07-23 note above (the fix only ever
touched `java.io.File.getAbsolutePath()`/`getAbsoluteFile()` in
`native-builtins/src/phases_late.rs`, never the `java.nio.file.Path`/
`Files` path this test actually exercises). The "confirming next step" from
2026-07-23 — trace whatever backs `Path` construction/`resolve()` for the
NIO API and apply the same trailing-separator strip there — is still the
right next step and is still un-attempted.

`ResourcesTests.whenAddDirectoryAndResourceAlreadyExistsThenIllegalStateExceptionIsThrown`
previously failed during its setup call:

```java
this.resources.addResource("one/two/three/", "content", true);
```

On Windows, CratonVM's synthetic `Path` representation retained the terminal
separator of an ordinary non-root path. `Files.writeString` then passed that
directory-shaped path to the Rust file-write bridge, and Windows returned
`ERROR_DIRECTORY` (267) instead of creating the file.

The fix canonicalizes trailing separators while constructing a synthetic host
`Path`: ordinary paths lose them, while drive, UNC, drive-less, and verbatim
roots keep them. Encoded jar/JRT filesystem paths are excluded because their
trailing slash is part of the virtual-entry representation.

The behavior was cross-checked against the local JDK 25 HotSpot implementation:
`root.resolve("one/two/three/").toString()` equals the no-trailing-separator
form, whereas `C:\\` and `\\localhost\\C$\\` retain their separators.

## Validation

- Native regression: `phases_late::p57_win_path_tests::trailing_separator_is_removed_only_from_non_roots` passed.
- `org.springframework.boot.testsupport.classpath.resources.ResourcesTests`: passed with JIT and `--nojit`.
- All eight compiled tests in `org.springframework.boot.testsupport.classpath.resources`: passed with JIT and `--nojit`.
