# Tomcat `DirResourceSet` case-sensitivity check silently defeated (FIXED 2026-07-04)

**Status:** FIXED. Commit `4b44910c` (`fix(io): restore Windows getCanonicalPath()
case-correction via targeted FindFirstFileW`), merged to `dev`.

## Symptom

`org.apache.catalina.webresources.AbstractTestResourceSet.testGetResourceCaseSensitive`
fails identically across all 6 concrete subclasses (`TestDirResourceSet`,
`TestDirResourceSetInternal`, `TestDirResourceSetMount`,
`TestDirResourceSetMountTrailing`, `TestDirResourceSetReadOnly`,
`TestDirResourceSetVirtual`): requesting `d1/d1-F1.txt` (wrong case; the real
file on disk is `d1-f1.txt`) incorrectly returns `exists()==true`.

## Investigation note: NOT an OSR bug, despite how it was originally reported

This bug was originally reported as a `CRATONVM_JIT_OSR=1`-exclusive regression
(PASS with OSR off, FAIL with OSR on), alongside two other suspected OSR
regressions in `jakarta.el` (`TestBeanSupport`, `TestImportHandlerStandardPackages`
— both already-known, unrelated, pre-existing bugs; not part of this fix).

Re-testing directly disproved the OSR attribution: the exact binary/commit the
bug was originally reported against fails **identically with `CRATONVM_JIT_OSR`
on or off**. The original "OSR off passes" baseline was a stale reference run
from 2026-06-29 — one day *before* the actual root cause (commit `5efeda8d`,
2026-06-30) landed. Comparing a post-regression OSR-on run against a
pre-regression OSR-off baseline created a false correlation with OSR.

## Root cause

Commit `5efeda8d` ("perf(io): string-based getCanonicalPath on Windows") changed
`File.getCanonicalPath()` on Windows from `std::fs::canonicalize` (which opens
the file via `GetFinalPathNameByHandleW`, real on-disk case included) to the
purely lexical `GetFullPathNameW` (no file open, to dodge a `cpcrypt.dll`
AppCompat filesystem-filter stall — see
[[reference_getcanonicalpath_getfullpathname_cpcrypt]]). `GetFullPathNameW`
does not query the filesystem, so it echoes back the caller's requested casing
verbatim instead of resolving the real on-disk filename case. That change's own
doc comment explicitly flagged this as an accepted tradeoff ("rarely matters
for Tomcat"), but Tomcat's `AbstractFileResourceSet.file()` (line ~179:
`canPath.equals(absPath)`) specifically depends on `getCanonicalPath()`
returning the true on-disk case to detect a case mismatch and reject it. With
the case-preserving canonicalize, `canPath` and `absPath` are always built from
the same (possibly wrong-case) input string, so the comparison can never
observe a mismatch — silently disabling the check on Windows.

## Fix

Added `win_case_correct()` (`native-builtins/src/phases_late.rs`, next to
`win_get_full_path_name`): after `GetFullPathNameW` produces an absolute,
lexically-normalized path, walk each existing path component and resolve its
real on-disk name via a **targeted, exact-name** `FindFirstFileW` probe against
the parent directory. `FindFirstFileW` enumerates directory entries — it never
opens the target file itself — so it stays immune to the `cpcrypt.dll` filter
that motivated moving off `std::fs::canonicalize` in the first place. A
non-existent path component (and everything nested below it) is left in the
caller's casing, matching HotSpot's canonicalize semantics for a path that
doesn't fully exist.

## Verification

- `testGetResourceCaseSensitive` (all 6 subclasses): FAIL → PASS, confirmed with
  `CRATONVM_JIT_OSR` both unset and `=1`.
- 23-class `org.apache.catalina.webresources.*` regression sweep: no new
  failures (two pre-existing, unrelated hangs — `TestAbstractArchiveResource`,
  `TestCachedResource` — confirmed present in the June 29 pre-regression
  baseline too).
- `TestCoyoteAdapterCanonicalization`: pre-existing hang, unchanged by this fix
  (also present in the June 29 baseline).
- Direct `File.getCanonicalPath()` probe (`d1/d1-F1.txt` → resolves to real
  on-disk `d1-f1.txt`).

## Repro

```
cd apps/tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName x `
  -Start 332 -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.webresources.TestDirResourceSet, index 332 in apps/tomcat/.suite/all-tests.txt
```
