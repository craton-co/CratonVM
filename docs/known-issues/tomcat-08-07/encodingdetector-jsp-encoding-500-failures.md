# TestEncodingDetector — 14/22 JSP-encoding tests return 500 instead of 200

**Status:** OPEN. **Severity:** medium-high (broad cluster within one class).
**HotSpot:** PASS.

## Summary

`org.apache.jasper.compiler.TestEncodingDetector` fails 14 of its 22 tests,
all with the same shape:
```
1) testEncodedJsp[1](org.apache.jasper.compiler.TestEncodingDetector)
java.lang.AssertionError: expected:<200> but was:<500>
...
Tests run: 22, Failures: 14
```
This is a parameterized test (`testEncodedJsp[N]`) that compiles/serves JSPs
written in various character encodings (likely covering combinations of
`page` directive `pageEncoding`/`contentType` charset, BOM presence/absence,
and multiple charset families — UTF-8, UTF-16, ISO-8859-1, etc., per
Jasper's `EncodingDetector` responsibility of sniffing a JSP source file's
encoding before parsing). 14 of 22 parameter combinations get a `500`
(server-side error, meaning the JSP failed to compile or threw during
encoding detection/parsing) where HotSpot returns `200`. The fact that 8/22
still pass suggests this is not a total breakage of `EncodingDetector` but a
specific subset of encodings/BOM combinations that CratonVM's encoding
detection or downstream compilation mishandles.

No further root-cause detail (specific failing parameter indices, exact
exception inside the 500 response) was extracted in this pass — the log
capture only preserved the outer HTTP-level assertion, not the underlying
Jasper compilation exception.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName encdetect `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.jasper.compiler.TestEncodingDetector
```

## Recommendation

Re-run this class alone and capture full server-side logs (the `500`
response almost certainly has a stack trace in the embedded Tomcat's log
output, not just the JUnit assertion) to identify the actual exception per
failing parameter. Cross-reference which specific `testEncodedJsp[N]`
indices fail vs pass to determine the failing encoding/BOM combination
pattern — this will likely point directly at a `java.nio.charset` or
`java.io.InputStreamReader`/`CharsetDecoder` difference in CratonVM's
real-JDK charset handling, similar in spirit to prior charset/decode gaps
found elsewhere in this codebase (e.g. `.par` classpath URI decode,
`reference_par_classpath_extension_uri_decode`).
