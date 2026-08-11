# Linux full-suite residual — 9 classes pass cleanly on real HotSpot, fail on CratonVM

**Status:** OPEN (2026-08-11). Azure Linux host, worktree
`/data/cratonvm-hib-linux-20260811` (branch `test/hib-linux-20260811`, dev
tip `54ce35df9` plus the local `run-hib.sh` portability fix), default
collector, H2 in-memory DB, real JDK 25.

## Method

After fixing the false-positive bytecode-enhancement sysprop gap (see
companion doc), the full 4579-class suite left 10 non-passed classes (7
FAIL, 2 HANG, 1 CRASH). Each was re-run individually under **real HotSpot**
(`/data/toolchain/jdk-25/bin/java`, same `common.args` classpath and
sysprops CratonVM uses, same `CratonRunner` JUnit5 launcher — a plain
vanilla `DiscoverySelectors.selectClass` + `LauncherFactory` call, nothing
CratonVM-specific) to confirm each failure is a genuine CratonVM-vs-HotSpot
divergence and not a pre-existing, environment-wide issue.

**One of the 10 turned out NOT to be a CratonVM bug** —
`ProxyPreservingFiltersOutsideInitialSessionTest` fails identically on both
VMs (`found=4 ok=1 failed=1 skipped=2` on both CratonVM and HotSpot) — byte-for-byte
matching counts, so it's excluded from the table below and not tracked
here.

## Confirmed genuine divergences (9 classes)

| Class | CratonVM | Real HotSpot (JDK 25) |
|---|---|---|
| `query.hql.FunctionTests` | FAIL: found=124 ok=94 **failed=24** skipped=6 | found=124 ok=118 failed=0 skipped=6 |
| `type.temporal.ZonedDateTimeTest` | **HANG**: rc=124, timeout=900s | found=608 ok=404 failed=0 aborted=204, ms=17051 |
| `query.hql.StandardFunctionTests` | FAIL: found=44 ok=39 **failed=5** | found=44 ok=44 failed=0 |
| `type.temporal.InstantTests` | **HANG**: rc=124, timeout=300s | found=204 ok=112 failed=0 aborted=92, ms=6708 |
| `sql.exec.SmokeTests` | FAIL: **found=0** failed=1 (class-level failure, no tests even started) | found=17 ok=16 failed=0 skipped=1, ms=4669 |
| `boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` | FAIL: found=0 failed=1, ms=618664 (10+ min before failing) | found=132 ok=132 failed=0, ms=24278 |
| `hql.ASTParserLoadingTest` | FAIL: found=106 ok=102 **failed=2** skipped=2 | found=106 ok=104 failed=0 skipped=2 |
| `bootstrap.scanning.JarVisitorTest` | **CRASH**: rc=0, no `@@RESULT` line printed despite a clean exit code | found=9 ok=9 failed=0, ms=1907 |
| `hql.HQLTest` | FAIL: found=169 ok=165 **failed=3** skipped=1 | found=169 ok=168 failed=0 skipped=1 |

## Cross-references to prior same-day findings

Most of these are not new defects in isolation — they match root causes
already under active investigation elsewhere this session, now confirmed to
reproduce on Linux too (same VM binary, different OS):

- **`DefaultCatalogAndSchemaTest`** — the long-running, still-open
  non-compacting-old-gen-arena fragmentation gap (see the Windows-side
  investigation the same day: the proxy-dispatch `Method`-object cache
  landed as a real fix for 9 other classes but does not resolve this one;
  root cause is a `StringWriter` buffer grow hitting fragmented old-gen
  space during DDL script accumulation, unrelated to the proxy fix).
- **`FunctionTests` / `ASTParserLoadingTest` / `HQLTest` / `StandardFunctionTests`**
  — all in the HQL-parser/query family already flagged this session as a
  recurring cluster (G1 TLAB-alignment-adjacent symptoms, ANTLR native-root
  interactions). Worth checking whether `79c302916` (the G1 TLAB fix) or a
  sibling fix touches these under the *default* collector too — these
  residuals are under the default collector here, not G1, so if the G1 fix
  doesn't apply this may be a distinct default-collector-only shape of the
  same family.
- **`ZonedDateTimeTest` / `InstantTests`** — join the existing temporal-type
  HANG family (`OffsetDateTimeTest`/`OffsetTimeTest` already have timeout
  floors in `class-overrides.tsv`); `InstantTests` in particular is new to
  this specific family and not previously seen hanging.
- **`SmokeTests`** — the class itself is a long-tracked residual
  (`testQueryConcurrency` throughput timeout, documented and RETIRED
  2026-07-23), but the *shape* here (`found=0`, a class-level failure before
  any test starts) does not match that already-retired doc's shape
  (`found=17 failed=1`, one test method timing out). Worth a fresh look —
  this may be a different failure on the same class, not a recurrence of
  the retired one.
- **`JarVisitorTest`** — genuinely new: `CRASH` with `rc=0` (a clean process
  exit code) but no `@@RESULT` line printed. Every other CRASH this session
  has been a nonzero rc (SIGSEGV, timeout). An rc=0 CRASH means the process
  exited normally but never reached the point in `CratonRunner.main` that
  prints the result line — worth checking whether this is a
  `System.exit()` called from deep inside the test itself (JarVisitorTest
  manipulates JAR/classpath scanning, a plausible place for a stray
  `System.exit`) short-circuiting the harness, rather than a VM bug.

## Repro (per class)

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home /data/toolchain/jdk-25
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner <className>
# control:
/data/toolchain/jdk-25/bin/java @common.args -Dcraton.batch=1 CratonRunner <className>
```
Common args must include the bytecode-enhancement sysprop fix from the
companion doc, or every class in this suite fails for an unrelated reason.

## Related

- `../../internal/fixed-suite-bugs/hibernate/azure-linux-common-args-missing-bytecode-enhancement-sysprop-20260811.md`
  — the harness config gap that produced the other 296 false positives this
  same run.
