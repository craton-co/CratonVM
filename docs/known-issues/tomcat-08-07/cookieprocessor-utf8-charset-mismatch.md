# TestCookieProcessorGenerationHttp — UTF-8 cookie value renders as `?` instead of the real character

**Status:** OPEN. **Severity:** medium (charset/encoding correctness).
**HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.tomcat.util.http.TestCookieProcessorGenerationHttp.testUtf8CookieValue`
fails:
```
1) testUtf8CookieValue(org.apache.tomcat.util.http.TestCookieProcessorGenerationHttp)
org.junit.ComparisonFailure: expected:<Test=[Ġ]> but was:<Test=[?]>
	at java.lang.AssertionError.<init>(AssertionError.java:76)
	at org.junit.ComparisonFailure.<init>(ComparisonFailure.java:37)
	at org.junit.Assert.assertEquals(Assert.java:117)
```
The test sets a cookie value containing a non-ASCII UTF-8 character (`Ġ`,
U+0120 LATIN CAPITAL LETTER G WITH DOT ABOVE) and checks the generated
`Set-Cookie` header round-trips it correctly. CratonVM's `CookieProcessor`
produces a literal `?` (the classic "unmappable character" replacement
symbol) instead — meaning somewhere in cookie-value generation, the UTF-8
byte sequence is being encoded/decoded through a charset that can't
represent `Ġ` (e.g. silently falling back to ASCII or Latin-1 / ISO-8859-1
instead of UTF-8) and substituting the replacement character rather than
preserving the original UTF-8 bytes.

Found via a fresh Linux rerun (dev commit `2335765e`, real JDK, JIT on,
300s timeout) on 2026-07-11. Verified via a fresh same-session HotSpot run:
PASSES on HotSpot.

## Reproduction

```bash
JH=/home/victor/jdk25
CP=$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)
"$CRATONVM_EXE" --java-home "$JH" -Xmx2g -cp "$CP" org.junit.runner.JUnitCore \
  org.apache.tomcat.util.http.TestCookieProcessorGenerationHttp
```

## Recommendation

Trace `org.apache.tomcat.util.http.CookieProcessor`'s (or the RFC6265/
`Rfc6265CookieProcessor`) cookie-value generation path for which
`Charset`/`String` encode-decode call converts the value — this has the
same shape as other charset-defaulting bugs found elsewhere in this
codebase (a call site expecting UTF-8 that CratonVM resolves to a
different default charset). Check whether
`Charset.defaultCharset()`/`file.encoding` resolution differs between
CratonVM and HotSpot in this specific code path, similar to prior findings
in the JSP-encoding-detection cluster
([encodingdetector-jsp-encoding-500-failures.md](encodingdetector-jsp-encoding-500-failures.md),
if still present) — worth checking whether this shares a root cause with
that cluster even though the symptom (silent `?` substitution vs. a JSP
500) differs.
