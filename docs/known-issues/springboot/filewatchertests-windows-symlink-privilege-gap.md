# `FileWatcherTests` 5/15 `FileSystemException` — Windows host lacks symlink privilege; not a CratonVM bug, confirmed still matching

**Status: CLOSED as not-a-bug (duplicate of existing, documented host limitation). Filed 2026-08-07 for the record.** Reconfirmed 2026-08-11 with a fresh Linux run (not just cited from history) — see below.

## Symptom

`craton-fullsuite-windows-20260806-s1`,
`core/spring-boot-autoconfigure` `org.springframework.boot.autoconfigure.ssl.FileWatcherTests`:
FAIL, 6.965s, 5 of 15 tests failed:

```
JUnit Jupiter:FileWatcherTests:shouldTriggerOnConfigMapUpdates(Path)          => java.nio.file.FileSystemException
JUnit Jupiter:FileWatcherTests:shouldFollowSymlinkRecursively(Path)           => java.nio.file.FileSystemException
JUnit Jupiter:FileWatcherTests:shouldFollowRelativePathSymlinks(Path)         => java.nio.file.FileSystemException
JUnit Jupiter:FileWatcherTests:shouldTriggerOnConfigMapAtomicMoveUpdates(Path) => java.nio.file.FileSystemException
JUnit Jupiter:FileWatcherTests:shouldFollowSymlink(Path)                      => java.nio.file.FileSystemException
SBRUNNER_RESULT tests=15 failed=5 aborted=0 skipped=0 containersFailed=0
```

Log: `apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-windows-20260806-s1/all-jit/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.ssl.F-dbf93449bdcf.out.log`.

## Checked against existing docs before treating as new — signature confirmed unchanged

Two existing docs cover this exact class:

- `files-createsymboliclink-unsupported-FIXED.md`
  (FIXED 2026-08-01) fixed `Files.createSymbolicLink` from an unconditional
  `UnsupportedOperationException` to a real implementation, and explicitly documents the
  **expected residual**: "That last one is what Windows reports without Developer Mode or an
  elevated token — 'A required privilege is not held by the client' — which is a *host*
  limitation HotSpot reports identically, not a missing feature," backed by a typed
  `FileSystemException(file, other, reason)` (repair #3 in that doc). Its own verification
  table lists `FileWatcherTests`'s 5 remaining (of 14 then-failing) as exactly this residual.
- `filewatcher-watchservice-surface-FIXED-20260801.md` (FIXED
  2026-08-01) fixed the `WatchService` surface itself (9 of the then-14 failures) and
  confirms the same split: "the other five are the separate `Files.createSymbolicLink` gap."
- `docs/known-issues/repros/nio-symlink/README.md` states the general rule directly:
  "Creating a symbolic link on Windows needs `SeCreateSymbolicLinkPrivilege` (an elevated
  token) or Developer Mode. Without either, `Files.createSymbolicLink` fails with
  `FileSystemException: ... A required privilege is not held by the client` on **stock
  HotSpot too**... That is also why the Spring Boot classes these probes stand in for
  (`ConfigTreePropertySourceTests`, `ApplicationTempTests`, `FileWatcherTests`) fail
  identically on both VMs on the Windows suite host."

**Verified this run's signature still matches**: exactly 5 failures, exactly the 5
symlink-dependent test names (`shouldFollowSymlink`, `shouldFollowSymlinkRecursively`,
`shouldFollowRelativePathSymlinks`, `shouldTriggerOnConfigMapUpdates`,
`shouldTriggerOnConfigMapAtomicMoveUpdates` — all symlink-tree setup/watch tests), all
`java.nio.file.FileSystemException` (the typed exception the 2026-08-01 fix introduced
specifically for this case, not the pre-fix `UnsupportedOperationException`), and the other
10 tests (including the non-symlink `WatchService` cases the 2026-08-01 fix repaired) pass.
No regression: this is the same, already-accepted residual, not a new or different failure
mode.

## Reconfirmed 2026-08-10 — collector-agnostic (Generational, G1, and ZGC)

Reconciling the 139-class union of non-passed classes from the three
2026-08-08f full-suite reruns (`craton-nonpassed-{default,g1,zgc}-20260808f`)
on `dev@6365de194`, `core/spring-boot-autoconfigure` `FileWatcherTests` is
FAIL under **all three** GC backends, byte-identical signature in every run:
5 of 15 tests failed, all `java.nio.file.FileSystemException`, exactly the
same 5 symlink-dependent test names as above:

| GC | Shard | Seconds | Log |
|---|---|---:|---|
| default (Generational) | s1 | 6.613 | `craton-nonpassed-default-20260808f-s1/all-jit/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.ssl.-dbf93449bdcf.{out,err}.log` |
| G1 | s1 | 6.734 | `craton-nonpassed-g1-20260808f-s1/all-jit/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.ssl.FileWatcherTests.{out,err}.log` |
| ZGC | s1 | 7.434 | `craton-nonpassed-zgc-20260808f-s1/all-jit/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.ssl.FileWatcherTests.{out,err}.log` |

(all under `apps/spring-boot-suite-runner/.suite/results/`). Same `15 tests
started`, `10 tests successful`, `5 tests failed`, same failing test
identifiers, same bare `=> java.nio.file.FileSystemException` (no message —
consistent with the `reason`-field-drop finding recorded in
`configtree-applicationtemp-windows-symlink-privilege-RETIRED-20260809.md`
§4). This is the same host-environment gap across all three collectors, not
a GC-specific defect — expected, since the root cause (host lacks
`SeCreateSymbolicLinkPrivilege`/Developer Mode) has nothing to do with the
collector. No action needed; conclusion below stands unchanged.

## Reconfirmed on Linux 2026-08-11 — genuinely CratonVM-vs-HotSpot-clean, not just "not chased"

Ran `FileWatcherTests` (plus the two sibling classes, `ConfigTreePropertySourceTests`
and `ApplicationTempTests`) on the Azure Linux host (`20.80.105.49`,
`/data/cratonvm-symlink-linux-20260811`, `dev@9f9c03f20`) rather than continuing
to cite the 2026-08-02 historical run — Linux needs no elevated privilege to
create a symlink, so this is the real pass/fail signal the Conclusion below
already said was missing.

**First attempt hit an unrelated, separate fixture bug**: `FileWatcherTests`
failed 15/15 with `NoClassDefFoundError: org/springframework/boot/autoconfigure/ssl/FileWatcher`
— nothing to do with symlinks. The shared fixture's
`core/spring-boot-autoconfigure/build/cratonvm-test-cp.txt` was missing that
module's own `build/classes/java/main` (the `.class` files exist on disk, no
jar was ever built for this module, and the classpath generator never fell
back to the raw classes dir). `ConfigTreePropertySourceTests`/`ApplicationTempTests`
(`core/spring-boot`) don't hit this — that module's classpath does include a
built jar. Confirmed this is a stale/incomplete classpath file, not a
CratonVM defect: patched a copy of the classpath file (appended the missing
`classes/java/main`/`resources/main` entries), reran, restored the original
file on the shared fixture immediately after (did not leave the shared
worktree modified). **Whoever owns that fixture next should regenerate
`cratonvm-test-cp.txt` for this module properly** (the Gradle init-script task
this repo already uses for it) rather than relying on this session's manual
patch, which was applied only for the duration of one test run.

**With the classpath gap worked around, the real result**: `FileWatcherTests`
**PASSES clean on Linux** (all 15 tests, 6.3s) — no symlink-privilege failures
at all, unlike the 5/15 residual on Windows. This directly confirms the
Conclusion's claim rather than just repeating it from a 9-day-old citation:
CratonVM's symlink implementation matches HotSpot on the platform where the
capability actually exists to test it. The two sibling classes were already
confirmed passing (`ConfigTreePropertySourceTests` 1.6s, `ApplicationTempTests`
0.8s) even before the classpath fix, since they don't touch the broken
module. All three: `/data/cratonvm-symlink-linux-20260811/apps/spring-boot-suite-runner/.suite/results/{symlink-verify-linux-20260811,filewatcher-cp-patched-20260811}/all-jit/`.

## Conclusion

Not a CratonVM bug and not a regression — this is the documented Windows-host-without-
Developer-Mode symlink-privilege gap, which fails identically on real HotSpot on this same
box. No action needed on the CratonVM side; would require either enabling Developer Mode /
running the suite host with `SeCreateSymbolicLinkPrivilege`, or running this class on Linux
(where `docs/known-issues/repros/nio-symlink/` reports the CratonVM implementation matches
HotSpot byte-for-byte), to get a real pass/fail signal from this specific coverage.

## Affected classes

- `core/spring-boot-autoconfigure` — `org.springframework.boot.autoconfigure.ssl.FileWatcherTests` (5/15 fail, host-environment limitation, duplicate of the residual documented in `files-createsymboliclink-unsupported-FIXED.md`)
