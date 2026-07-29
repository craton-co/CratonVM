# ResourcesTests: Windows trailing-separator path normalization

**Status: RESOLVED 2026-07-18**

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
