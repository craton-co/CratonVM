# Mapper package — performance-threshold and welcome-file-redirect failures

**Status:** OPEN. **Severity:** low-medium (one perf-threshold assertion, one
functional redirect-vs-200 mismatch). **HotSpot:** PASS on both.

## `TestMapperPerformance.testPerformance` — threshold assertion, raw number

```
1) testPerformance(org.apache.catalina.mapper.TestMapperPerformance)
java.lang.AssertionError: 167375
```
A bare numeric assertion message (no "expected/but was" wrapper) — this is
almost certainly a `assertTrue(elapsedNanos < threshold, "" + elapsedNanos)`-
style perf-timing test, where `167375` is the actual measured value (likely
microseconds or nanoseconds) that exceeded some hardcoded threshold. Given
this session's established pattern of `*Performance`-suffixed classes being
timing/scheduler-sensitive rather than functional bugs (see the earlier
Windows-vs-Linux cross-platform comparison, where several `*Performance`
classes hung on Windows only), **this may be host-load/timing variance
rather than a genuine regression** — worth a low-priority re-check on a
quiet host before investing further.

## `TestMapperWebapps.testWelcomeFileStrict` — welcome-file redirect mismatch

```
1) testWelcomeFileStrict(org.apache.catalina.mapper.TestMapperWebapps)
java.lang.AssertionError: expected:<200> but was:<302>
```
A request that should be served directly (`200`) with strict welcome-file
mapping instead gets redirected (`302`). This looks like a genuine, specific
functional difference in Tomcat's `Mapper`/welcome-file resolution logic
under CratonVM — worth investigating independently of the performance
finding above.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName mapperfail `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.mapper.TestMapperWebapps  (testWelcomeFileStrict)
# org.apache.catalina.mapper.TestMapperPerformance
```

## Recommendation

For `TestMapperWebapps`: read `testWelcomeFileStrict`'s webapp fixture
(likely under `test/webapp*/`) to see the exact welcome-file config, then
compare CratonVM's `Mapper.map()` resolution against HotSpot's for that
specific URL — the `strict` variant matters (Tomcat has both strict and
lenient welcome-file matching modes; check whether CratonVM's env/config
plumbing is picking the wrong mode, or the strict-mode logic itself differs).
For `TestMapperPerformance`: re-run in isolation on an idle host before
treating as a real regression.
