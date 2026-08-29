# `TestFileStore` — directory reads as empty (NOT A CRATONVM BUG)

**Status: CLOSED — 2026-08-29. Not reproducible; environmental shared-fixture hazard.**

## Original hypothesis, and why it doesn't hold

The original doc proposed two mechanisms: (1) `TesterServletContext`'s
`ServletContext.TEMPDIR` attribute resolving non-null on CratonVM where
HotSpot leaves it null, or (2) CratonVM's `File(File, String)` two-arg
constructor / `File.list()` on a freshly-`mkdir()`'d relative path behaving
differently than HotSpot's.

Both are refuted by reading the actual code and by direct measurement:

- `TesterServletContext.getAttribute(String)`
  (`apps/tomcat/test/org/apache/tomcat/unittest/TesterServletContext.java:150`)
  unconditionally `return null;` — `TEMPDIR` is null on **both** VMs. There is
  nothing to diverge on here.
- CratonVM's `File(File, String)` native override
  (`native-builtins/src/phases_late/nio_file.rs`, the
  `<init>(Ljava/io/File;Ljava/lang/String;)V` registration) already documents
  and implements the correct null-parent rule: `new File((File) null, "x")`
  normalises to `"x"` (relative), exactly like `new File("x")` — it does
  **not** resolve against a default parent the way `new File("", "x")` does.
  A standalone probe (`FileNullParentProbe.java`) confirms this end to end
  under CratonVM: `File((File)null,"SESS_TEMP")` and `File("SESS_TEMP")`
  produce the identical path string, and `exists()`/`isDirectory()`/`list()`
  on the null-parent-resolved `File` see exactly the files the test itself
  created — matching HotSpot exactly.
- The real Tomcat test class, `org.apache.catalina.session.TestFileStore`,
  run via `run-tomcat-suite.sh`'s exact invocation against the current
  `livedbg` build, passes cleanly and repeatably: **OK (5 tests)**, 4
  consecutive runs, 0 failures.

## What actually produces the symptom

`TestFileStore` writes and reads a **CWD-relative** directory,
`SESS_TEMP`, with no per-run isolation: `@BeforeClass` doesn't clean it, and
`@AfterClass` deletes it only on a clean exit. If `SESS_TEMP` already
contains files when the class starts — a stale leftover from an interrupted
prior run, or (on this host) a concurrent process sharing the same
`TC_ROOT` — the class's `getSize()`/`keys()` assertions fail because they see
more (or different) files than the two the current run created.

This was reproduced directly, **identically on both VMs**, by pre-seeding
`SESS_TEMP` with an extra file before running the class:

```
$ touch SESS_TEMP/stale.session SESS_TEMP/other.txt
$ java -cp $CP org.junit.runner.JUnitCore org.apache.catalina.session.TestFileStore     # HotSpot
FAILURES!!! Tests run: 5, Failures: 2
  keys(): array lengths differed, expected.length=2 actual.length=3;
  expected:<[tmp1]> but was:<[stale]>

$ cratonvm ... org.junit.runner.JUnitCore org.apache.catalina.session.TestFileStore     # CratonVM
FAILURES!!! Tests run: 5, Failures: 2
  (identical failure)
```

The two VMs fail identically, for the identical reason, under the identical
contamination. This is exactly the shape a shared, CWD-relative test fixture
produces on a host running many concurrent worktrees against the same
vendored `apps/tomcat` checkout (`/data/cratonvm/apps/tomcat`, referenced by
every worktree that symlinks or points `TC_ROOT` at it) — not a VM defect.
The original "0 files where 2 exist" symptom is consistent with a concurrent
run's `@AfterClass` cleanup (`ExpandWar.delete(dir)`) landing between this
run's file creation and its assertion.

## Validation

Azure host, worktree `/data/cvm-tomcatfs-20260829`, `livedbg` build,
`TC_ROOT=/data/cratonvm/apps/tomcat`:

- Clean `SESS_TEMP` (deleted before each run), 4 consecutive runs of
  `org.apache.catalina.session.TestFileStore`: **OK (5 tests)** every time.
- `FileNullParentProbe.java` (isolated `File(File,String)`/`exists`/
  `isDirectory`/`list` check): matches HotSpot exactly.
- Adversarial repro with a pre-seeded stale file in `SESS_TEMP`: fails
  identically on HotSpot and CratonVM (same assertion, same mismatched
  element).

No CratonVM code change made. No known residual remains for this class.
