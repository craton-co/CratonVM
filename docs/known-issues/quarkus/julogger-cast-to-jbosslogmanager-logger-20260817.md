# `java.util.logging.Logger` cannot be cast to `org.jboss.logmanager.Logger` — FIXED 2026-08-17

**Status: FIXED and landed on `dev`.** Found and fixed 2026-08-17 (Azure
host, `cratonvm-quarkus-zgc`, dev base `496bc3c2c`), committed and pushed
same session as `04483c5ee` (`fix/jboss-logmanager-getlogger-dispatch-20260817`,
fast-forward merged to `dev`). Was the dominant blocker in a
quarkus full-suite rerun — ~78% of all recorded results, 99/105 `NOSTART`
classes failing identically:

```
<clinit> failed — wrapping in ExceptionInInitializerError
  class=io/quarkus/test/AbstractQuarkusExtensionTest
  cause=java.lang.ClassCastException: class java.util.logging.Logger cannot
    be cast to class org.jboss.logmanager.Logger (java.util.logging.Logger
    is in module java.logging of loader 'bootstrap'; org.jboss.logmanager.
    Logger is in module org.jboss.logmanager of loader 'app')
```

`AbstractQuarkusExtensionTest.java:112`:
`rootLogger = (Logger) LogManager.getLogManager().getLogger("")`, where
`Logger` is `org.jboss.logmanager.Logger`.

## Two independent gaps, not one

**1. The harness never set `java.util.logging.manager`.** `common.args`
had no `-Djava.util.logging.manager=org.jboss.logmanager.LogManager` — real
Maven Surefire injects this for every quarkus test module; this hand-rolled
`CratonRunner`-based harness never replicated it. **Verified this alone is
not CratonVM-specific**: a minimal repro
(`LogManager.getLogManager().getLogger("")` cast to
`org.jboss.logmanager.Logger`) fails identically on stock HotSpot 25 without
the flag (`CAST_FAILED` on both VMs). Fixed by appending the flag to
`common.args` (original backed up as `common.args.orig-before-logmanager-fix-20260817`).

**2. With the flag set, a genuine CratonVM defect remained.** HotSpot: the
flag correctly makes `LogManager.getLogManager()` return an
`org.jboss.logmanager.LogManager` instance AND makes `.getLogger("")` return
an `org.jboss.logmanager.Logger`. CratonVM: the manager-class swap worked
(`LogManager impl class = org.jboss.logmanager.LogManager` — correct!), but
`.getLogger("")` still handed back a plain `java.util.logging.Logger`
(`CAST_FAILED` persisted with the flag set, CratonVM-only).

## Root cause

CratonVM doesn't run real `java.util.logging`/`org.jboss.logmanager`
bytecode for logger creation — `native-builtins/src/logmanager.rs` and
`jboss_logmanager.rs` reimplement both natively, including a *second*,
already-correct code path (`get_or_create_jboss_logger`, backing
`org.jboss.logmanager.LogContext.getLogger` — JBoss's own internal entry
point) that allocates a proper `org/jboss/logmanager/Logger`-shaped
synthetic object.

The bug: `CLS_JBOSS_LOG_MANAGER`'s `getLogger` native registration pointed
at the *same* `native_get_logger` function as the plain JUL manager
(`getLogger` is inherited unchanged from `java.util.logging.LogManager`, so
nothing had ever given it a JBoss-specific override) — which unconditionally
allocates a `CLS_JUL_LOGGER`-shaped object regardless of which manager
singleton is active. The correct JBoss-shaped allocator existed and worked;
it just wasn't wired to the path real application code calls.

## Fix

`native-builtins/src/logmanager.rs`:
- Added `native_get_jboss_manager_logger`, a thin wrapper around the
  existing (already-correct) `get_or_create_jboss_logger`.
- Changed `CLS_JBOSS_LOG_MANAGER`'s `getLogger` registration from
  `native_get_logger` to the new function. `CLS_JUL_LOG_MANAGER`'s
  registration is untouched.

`apps/quarkus-suite-runner/common.args`:
- Appended `-Djava.util.logging.manager=org.jboss.logmanager.LogManager`.

## Verification

- `cargo test -p cratonvm-native-builtins logmanager` (lib target): 30/30
  pass, no regressions.
- Minimal repro (`LogManager.getLogManager().getLogger("")` cast to
  `org.jboss.logmanager.Logger`): `CAST_OK` on both HotSpot and the fixed
  CratonVM binary, with the flag set.
- Real quarkus test classes (`io.quarkus.aesh.deployment.CliConfigPromptTest`,
  `CommandBeanRegistrationTest`, `CliSettingsCustomizerTest`,
  `CdiInjectionInCommandTest`): **zero** `ClassCastException` occurrences in
  any raw log — the specific defect this doc tracks is confirmed gone.

## What this fix exposed next

Those same 4 classes still don't reach `started>0` — they now progress
noticeably further through JUnit5 bootstrap (past `LauncherSessionListener`,
`TestEngine`, and `PostDiscoveryFilter` loading) and stop at
`io.quarkus.test.junit.util.QuarkusTestProfileAwareClassOrderer`
(JUnit5's test-class-ordering step, run during discovery, before any test
executes). This is a **new, separate** blocker one layer further into the
same bootstrap chain — same shape as this whole investigation so far
(`LoggingSetupRecorder` → this Logger-cast bug → now this). Not yet
characterized; worth its own doc once isolated.

## Related

- `internal/fixed-suite-bugs/quarkus/loggingsetuprecorder-nosuchmethoderror-at-classpath-scale-20260817.md`
  — the prior blocker in the same bootstrap chain, retired 2026-09-01. The
  binary that page ran was five days stale, but the defect it saw was real:
  the Quarkus logging native wrote down its own copy of
  `LoggingSetupRecorder.initializeLogging`'s descriptor, and `70248949c`
  (2026-08-13) is the fix. The wording that used to stand here — "a stale
  binary, not a live defect" — is true of 08-17, not of the bug.
- `internal/fixed-suite-bugs/wildfly/wildfly-jboss-logmanager-geteffectivelevel-null-loggernode.md` —
  an earlier, different jboss-logmanager native-shim gap in the same file
  family.
