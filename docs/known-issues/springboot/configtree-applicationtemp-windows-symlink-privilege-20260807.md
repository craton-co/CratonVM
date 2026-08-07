# `ConfigTreePropertySourceTests` + `ApplicationTempTests` — Windows host lacks `SeCreateSymbolicLinkPrivilege`, not a CratonVM bug

**Status: OPEN — accepted host limitation, confirmed 2026-08-07 (not a
regression, not CratonVM-specific).**

## Symptom (2026-08-06 full-suite Windows run, `craton-fullsuite-windows-20260806-s1`)

| Class | Status | Seconds | tests | failed | aborted | skipped |
|---|---|---:|---:|---:|---:|---:|
| `core/spring-boot` `org.springframework.boot.env.ConfigTreePropertySourceTests` | FAIL | 4.311 | 23 | 3 | 0 | 0 |
| `core/spring-boot` `org.springframework.boot.system.ApplicationTempTests` | FAIL | 2.062 | 5 started (6 found) | 0 | 1 | 1 |

`ConfigTreePropertySourceTests` fails 3 tests outright:

```
Failures (3):
  JUnit Jupiter:ConfigTreePropertySourceTests:getPropertyNamesFromNestedWithSymlinkInPathReturnsPropertyNames()
    => java.nio.file.FileSystemException
  JUnit Jupiter:ConfigTreePropertySourceTests:getPropertyNamesFromFlatWithSymlinksIgnoresHiddenFiles()
    => java.nio.file.FileSystemException
  JUnit Jupiter:ConfigTreePropertySourceTests:getPropertyNamesFromNestedWithSymlinksIgnoresHiddenFiles()
    => java.nio.file.FileSystemException
SBRUNNER_RESULT tests=23 failed=3 aborted=0 skipped=0 containersFailed=0
```

`ApplicationTempTests` shows `failed=0` — the runner still marks it `FAIL`
because its verdict rule is `failed>0 OR aborted>0 OR containersFailed>0`
(`run-spring-boot-suite.ps1:883`), and this class has `aborted=1`:

```
[         6 tests found           ]
[         1 tests skipped         ]
[         5 tests started         ]
[         1 tests aborted         ]
[         4 tests successful      ]
SBRUNNER_RESULT tests=5 failed=0 aborted=1 skipped=1 containersFailed=0
```

Logs:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-windows-20260806-s1/all-jit/logs/core_spring-boot.org.springframework.boot.env.ConfigTreePropertySourceTests.{out,err}.log`,
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-windows-20260806-s1/all-jit/logs/core_spring-boot.org.springframework.boot.system.ApplicationTempTests.{out,err}.log`.

## Root cause — confirmed, source-level, not CratonVM-specific

Both classes call `java.nio.file.Files.createSymbolicLink` directly against
the real filesystem:

- `ConfigTreePropertySourceTests` (`apps/spring-boot/core/spring-boot/src/test/java/org/springframework/boot/env/ConfigTreePropertySourceTests.java:99,265-267,289-291`)
  calls `Files.createSymbolicLink(...)` with **no try/catch** — any failure
  propagates straight up as a test failure. All 3 failing tests go through
  `getSymlinkedFlatPropertySource()`/`getSymlinkedNestedPropertySource()`/the
  inline symlink-in-path case, all of which call it unconditionally.
- `ApplicationTempTests.whenSymlinkExistsInDirectoryLocationGetDirThrows()`
  (`apps/spring-boot/core/spring-boot/src/test/java/org/springframework/boot/system/ApplicationTempTests.java:106-120`)
  is more defensive — it wraps the same call:
  ```java
  try {
      Files.createSymbolicLink(path, linkTarget);
  }
  catch (Exception ex) {
      Assumptions.abort("Symlink creation not supported");
  }
  ```
  which is exactly why this one shows up as `aborted=1`, not `failed=1`: the
  test itself detects the platform limitation and calls
  `Assumptions.abort(...)`, which JUnit reports as an abort, not a failure —
  it's only the suite runner's own `aborted>0 ⇒ FAIL` verdict rule that turns
  it into a row worth triaging at all.

`Files.createSymbolicLink` requires either `SeCreateSymbolicLinkPrivilege`
(an elevated token) or Windows Developer Mode. **This suite-runner host has
neither**, so the call fails identically on real HotSpot JDK 25 too — this
was already established and recorded during the 2026-07-17 rerun (first time
this suite got a same-scope HotSpot baseline):

> Two classes (`ConfigTreePropertySourceTests`, `FileWatcherTests`) fail on
> both VMs identically with a Windows `FileSystemException` creating a
> symlink in temp — this host lacks `SeCreateSymbolicLinkPrivilege`/Developer
> Mode. A genuine host-level limitation, not a JDK or CratonVM difference;
> not fixed (affects both VMs equally, so it self-filters out of the
> CratonVM-specific bucket below regardless).
> — `apps/spring-boot-suite-runner/RESULTS-20260717.md` §"Three runner bugs
> found + fixed this round", item 3.

Also documented, independent of any specific test class, in
[`docs/known-issues/repros/nio-symlink/README.md`](../repros/nio-symlink/README.md):

> Creating a symbolic link on Windows needs `SeCreateSymbolicLinkPrivilege`
> (an elevated token) or Developer Mode. Without either, `Files.createSymbolicLink`
> fails with `FileSystemException: ... A required privilege is not held by
> the client` on **stock HotSpot too**... That is also why the Spring Boot
> classes these probes stand in for (`ConfigTreePropertySourceTests`,
> `ApplicationTempTests`, `FileWatcherTests`) fail identically on both VMs on
> the Windows suite host.

`ApplicationTempTests`'s `err.log` and `out.log` contain nothing beyond the
standard VM-boot "Post-clinit fixup" lines and a clean `System.exit(0)` —
no exception text is printed for the aborted test because `Assumptions.abort`
doesn't produce a stack trace, it just short-circuits that one test method.
`ConfigTreePropertySourceTests`'s `err.log` is similarly unremarkable (just
the boot fixups and a `System.exit(1)`); the `FileSystemException` detail
lives entirely in the JUnit summary printed to `.out.log`.

## Why this is not filed as a CratonVM bug

- Both classes fail/abort for exactly the reason their own test authors
  anticipated (`ApplicationTempTests` explicitly catches and downgrades the
  failure to an assumption-abort; `ConfigTreePropertySourceTests`'s three
  affected tests are testing symlink-aware behavior and cannot meaningfully
  run without real symlink support).
- The failure mode (`FileSystemException`, "a required privilege is not
  held by the client") is a Windows ACL/privilege check enforced by the OS
  kernel before either JVM's `java.nio.file` implementation gets involved —
  there is no code path in CratonVM's NIO layer that could paper over it
  without literally requesting the elevated privilege on the process's
  behalf, which is out of scope for a test-suite host.
- `ApplicationTempTests`'s `aborted=1`/`skipped=1` breakdown matches the test
  source exactly: 6 `@Test` methods, 1 `@DisabledOnOs(OS.WINDOWS)` (skipped),
  1 that self-detects the symlink gap and aborts, 4 that need no symlink
  support and pass normally. This is the expected shape on a Windows host
  without Developer Mode on **any** JVM, not a CratonVM-specific count.

## What would close it (not attempted here — host configuration, not code)

Enable Developer Mode on the Windows suite-runner host (`Settings → Privacy
& Security → For developers → Developer Mode`), or run the grant token with
`SeCreateSymbolicLinkPrivilege`, or move this class of test to a Linux
shard where the privilege isn't required. None of these are VM code changes;
this doc exists to prevent re-triaging the same host gap as a fresh
CratonVM regression on future full-suite runs.

## Related

- `FileWatcherTests` was named alongside these two in the 2026-07-17 finding
  but is not part of this run's 5-class cluster; if it recurs, it is the same
  root cause.
- `docs/known-issues/repros/nio-symlink/README.md` — standalone dual-VM
  symlink probes (`SymlinkProbe.java`, `SymlinkWalkProbe.java`), useful for
  re-confirming this on a differently-configured host without running the
  full Spring Boot suite.
