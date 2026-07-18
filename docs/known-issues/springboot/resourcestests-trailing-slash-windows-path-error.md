# `ResourcesTests` — a resource name with a trailing `/` reaches `Files.writeString` unstripped, producing a Windows `ERROR_DIRECTORY` instead of writing a file

**Status: OPEN — found 2026-07-17 (hypothesis, not confirmed against native `java.nio.file` source)**

## Symptom

| Class | Failures |
|---|---:|
| `org.springframework.boot.testsupport.classpath.resources.ResourcesTests` | 1/12 |

```
JUnit Jupiter:ResourcesTests:whenAddDirectoryAndResourceAlreadyExistsThenIllegalStateExceptionIsThrown()
    MethodSource [className = 'org.springframework.boot.testsupport.classpath.resources.ResourcesTests', methodName = 'whenAddDirectoryAndResourceAlreadyExistsThenIllegalStateExceptionIsThrown', methodParameterTypes = '']
    => java.lang.IllegalStateException: IOException: Неверно задано имя папки. (os error 267)
       org.springframework.boot.testsupport.classpath.resources.Resources.addResource(Resources.java:114)
       org.springframework.boot.testsupport.classpath.resources.ResourcesTests.whenAddDirectoryAndResourceAlreadyExistsThenIllegalStateExceptionIsThrown(ResourcesTests.java:146)
```

("Неверно задано имя папки" = Windows' localized text for error 267,
`ERROR_DIRECTORY` — "The directory name is invalid".)

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/test-support_spring-boot-test-support.org.springframework.boot.testsupport.classpath.resources-c869c3f8368d.out.log`

## Root cause (hypothesis, grounded in the failing call but not confirmed at the native `Path`/`Files` implementation level)

The failing test (`ResourcesTests.java:145-148`):

```java
void whenAddDirectoryAndResourceAlreadyExistsThenIllegalStateExceptionIsThrown() {
    this.resources.addResource("one/two/three/", "content", true);
    assertThatIllegalStateException().isThrownBy(() -> this.resources.addDirectory("one/two/three"));
}
```

fails on the **first line** — the `addResource` setup call itself throws,
before the test even reaches its actual `assertThatIllegalStateException()`
assertion. Its resource name is `"one/two/three/"`, with a **trailing
slash**, unlike the structurally-identical sibling test just below it
(`whenAddResourceAndDirectoryAlreadyExistsThenIllegalStateExceptionIsThrown`,
which does `addDirectory("one/two/three")` — no trailing slash — and
passes).

`Resources.addResource` (`test-support/spring-boot-test-support/src/main/java/org/springframework/boot/testsupport/classpath/resources/Resources.java:103-121`):

```java
Resources addResource(String name, String content, boolean additional) {
    Path resourcePath = this.root.resolve(name);
    ...
    Files.createDirectories(parent);
    Files.writeString(resourcePath, processContent(content));   // line 114 — throws
    ...
}
```

`this.root.resolve("one/two/three/")` should, per `java.nio.file.Path`
semantics, normalize away the trailing separator when producing the
resolved path's string form (real `WindowsPath` construction strips
trailing `\`/`/` except for a bare root) — so `Files.writeString` should
receive a plain file path (`.../one/two/three`) and write a regular file
named `three`, exactly like the no-trailing-slash sibling test does. The
Windows-specific `ERROR_DIRECTORY` (267) response strongly suggests that on
CratonVM the trailing separator is **not** stripped somewhere between
`Path.resolve` and the underlying file-write syscall, so the OS receives a
path that still ends in `\` — which Windows' `CreateFile`
(or equivalent) rejects for a *file*-write operation with exactly this
error, since a trailing separator asserts "this is a directory".

**Not confirmed:** this session did not trace CratonVM's `java.nio.file.Path`/
`Files.writeString` native implementation to find the exact point the
trailing separator survives (whether in `Path.resolve`'s own
Windows-path-string normalization, or in whatever native syscall wrapper
backs `Files.writeString`/`FileChannel.open` on Windows). Confirming would
mean a standalone repro:
`Paths.get("C:\\tmp").resolve("a/b/").toString()` on CratonVM vs. real
HotSpot (expect `...\a\b` on HotSpot; if CratonVM instead yields
`...\a\b\`, that pins the bug to `Path.resolve`'s string normalization
specifically), or tracing the native file-write call if `resolve` turns
out to be correct and the separator survives elsewhere.

Note the suite is run on Windows for both CratonVM and the HotSpot
baseline (this repo's `env.md`/task context: same host, same day), so this
is not a HotSpot-vs-CratonVM platform difference — the baseline passes this
exact test on the same OS.

## Affected classes

| Module | Class |
|---|---|
| `test-support/spring-boot-test-support` | `org.springframework.boot.testsupport.classpath.resources.ResourcesTests` |
