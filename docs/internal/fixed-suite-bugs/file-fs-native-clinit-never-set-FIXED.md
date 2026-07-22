# `java.io.File.FS` never set by native `<clinit>` override — FIXED

**Status:** FIXED. **Found/fixed:** 2026-07-09, while investigating the
Tomcat WebSocket close-delay bug (see
`../../known-issues/tomcat/wsremoteendpoint-close-delay-near-deadlock.md`).
Commit: see `git log --oneline -- native-builtins/src/lib.rs` for
`native_file_clinit` on `dev`.

## Summary

`native-builtins/src/lib.rs` registers a native override for
`java/io/File.<clinit>()V` (`native_file_clinit`, unconditional — real-JDK
and synthetic-JDK modes both hit it, since `<clinit>` dispatch checks the
native registry regardless of `check_override`/abstractness). This override
completely replaces the real bytecode clinit — it backfills
`separator`/`separatorChar`/`pathSeparator`/`pathSeparatorChar` but **never
sets `FS`** (the `private static final FileSystem FS` field, JVMS-wise the
*first* static assigned in the real bytecode, before the separator fields).

Any real-JDK `File` instance method NOT covered by the `check_override`
allow-list (`<init>`, `getAbsolutePath`, `getCanonicalPath`, `exists`,
`isFile`, `isDirectory`, `getPath`, `toPath`, `getName`, `toURI`) runs real
bytecode, and several of those (e.g. the private `isInvalid()` helper used
by `length()`, `lastModified()`, `delete()`, `mkdir()`, `list()`, …)
dereference `FS` directly (`FS.isInvalid(this)`). With `FS` permanently
null, every such call threw:

```
java.lang.NullPointerException: Cannot invoke
"java.io.FileSystem.isInvalid(java.io.File)" because "java.io.File.FS" is null
```

First observed via Apache Tomcat's `Digester`/`SAXParserFactory` internals
statting a `File` through a non-overridden path while loading
`mbeans-descriptors.xml` during `Server`/`Engine` bootstrap — 100%
reproducible on every real-JDK-mode Tomcat `Tomcat.start()` call, cascading
into `StandardContext startup failed due to previous errors` and blocking
the entire `TestWsRemoteEndpointImplServerDeadlock` repro before it could
even reach the code path under test. Does **not** reproduce in
synthetic-JDK mode (most `File` operations there route through the
`check_override`-forced natives, which never read `FS`).

## Root cause

`vm/src/vm/vm_util.rs`'s `post_clinit_fixup` (success-path fixup for
`"java/io/File"`) only backfills the four separator fields — it was written
under the (now-stale, see below) assumption that the real bytecode clinit
runs and only *fails partway through* (a swallowed-`<clinit>` scenario per
JVMS §5.5). That's not what happens: `native_file_clinit` is a *full*
`<clinit>` replacement, so the fixup's separator backfill is the only thing
that ever sets those four fields, and `FS` was simply never assigned by
anything.

## Fix

`native_file_clinit` (native-builtins/src/lib.rs) now also constructs and
publishes a real filesystem object for `File.FS`:
- Linux/non-Windows: `java/io/UnixFileSystem` with `slash`/`colon`/`userDir`
  populated by name (`ctx.set_field_by_name`).
- Windows: `java/io/WinNTFileSystem` with `slash`/`semicolon`/`altSlash`/
  `userDir`.

Using `set_field_by_name` (not hardcoded numeric field indices) makes this
robust regardless of the real class's exact field layout — see the sibling
`threadgroup-native-field-index-mismatch-FIXED.md` fix, which was needed
for exactly the opposite mistake (hardcoded indices that didn't match the
real layout).

## Verification

- `FileProbe.java` (20 concurrent threads, each `new File("/tmp").exists()`)
  — 0 failures before and after (this path was already using
  `check_override`-forced natives, unaffected either way).
- Manual repro: any `File` method that hits `isInvalid()` via real bytecode
  (e.g. constructing a `Digester`/`SAXParserFactory` under real-JDK mode)
  no longer throws `NullPointerException: ... "java.io.File.FS" is null`.
- Full Tomcat `TestWsRemoteEndpointImplServerDeadlock` repro: the
  `Digester`/`mbeans-descriptors.xml` NPE cascade is gone after this fix
  (confirmed via repeated runs — 0/several occurrences post-fix vs.
  consistently ~15-20 occurrences per run pre-fix).

Not independently regression-tested against the full suite (time-boxed
alongside the ThreadGroup fix below, both discovered as blockers for the
same investigation) — recommend a broader real-JDK-mode Tomcat/File-heavy
suite rerun on `dev` to confirm no fallout.
