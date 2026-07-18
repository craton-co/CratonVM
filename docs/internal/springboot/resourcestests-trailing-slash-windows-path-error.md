# ResourcesTests: Windows trailing-separator path normalization

**Status: RESOLVED 2026-07-18**

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
