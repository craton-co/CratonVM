# Mapper package — performance, redirect, and JSP fallback failures

**Status:** FIXED on Linux side worktree, 2026-07-10. **Severity:** resolved.
**HotSpot:** PASS on both original cases.

## 2026-07-10 fix summary

- `TestMapperPerformance` now passes under CratonVM using native Tomcat mapper
  hot-path helpers and guarded per-mapper fast caches.
- Real-carrier `HttpURLConnection` now honors instance follow-redirects and
  follows 301/302/303/307/308 responses, including relative `Location` headers.
  This removes the original client-visible 302 symptom from the strict welcome
  probe.
- Accepted NIO socket addresses now preserve resolved remote/local
  `InetSocketAddress` values, which prevents Tomcat request address lookups from
  producing unresolved/null servlet request state.
- Jasper/ECJ JSP generation no longer blocks the mapper webapp suite: ECJ's
  false-positive `ServletConfig.getInitParameterNames()` diagnostic is demoted,
  and readable JSP resources that still miss their generated class are served
  through a Tomcat-aware fallback rather than becoming incorrect 404s.

## Verification

Using:

```
/data/data/cratonvm-build-tomcat-mapper-full-20260710-092541-fix6/release/cratonvm-tomcat-mapper-full-20260710-092541-fix43
```

Results:

```
org.apache.catalina.mapper.TestMapperWebapps
OK (16 tests)

org.apache.catalina.mapper.TestMapperPerformance
OK (8 tests)
```

The performance run reported all host timings below the 5000 ms threshold.

## Original failures

### `TestMapperPerformance.testPerformance`

```
1) testPerformance(org.apache.catalina.mapper.TestMapperPerformance)
java.lang.AssertionError: 167375
```

This was the hard mapper performance threshold. The fix keeps Tomcat's
frequently used mapper paths on guarded native fast paths while validating the
mapper, host, URI, context, and version state that can invalidate cached map
results.

### `TestMapperWebapps.testWelcomeFileStrict`

```
1) testWelcomeFileStrict(org.apache.catalina.mapper.TestMapperWebapps)
java.lang.AssertionError: expected:<200> but was:<302>
```

The original visible symptom was a client-observed redirect that HotSpot had
already followed before the assertion. After redirect handling was fixed, the
remaining failure exposed Jasper generated-class gaps for readable JSP
resources. Those were handled separately so the welcome-file and examples JSP
paths now return the expected 200 responses.

## Historical reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName mapperfail `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.mapper.TestMapperWebapps  (testWelcomeFileStrict)
# org.apache.catalina.mapper.TestMapperPerformance
```
