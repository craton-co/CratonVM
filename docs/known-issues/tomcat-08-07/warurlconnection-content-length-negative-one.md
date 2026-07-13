# TestWarURLConnection — getContentLength() returns -1 instead of the real size

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.catalina.webresources.war.TestWarURLConnection.testContentLength`
fails:
```
1) testContentLength(org.apache.catalina.webresources.war.TestWarURLConnection)
java.lang.AssertionError: expected:<137> but was:<-1>
	at java.lang.AssertionError.<init>(AssertionError.java:76)
	at org.junit.Assert.fail(Assert.java:89)
	at org.junit.Assert.failNotEquals(Assert.java:835)
	at org.junit.Assert.assertEquals(Assert.java:647)
```
This tests Tomcat's custom `URLConnection` implementation for reading a
resource out of a `.war` file (a jar-in-jar style nested-archive URL
scheme). `getContentLength()` should return the entry's real uncompressed
size (137 bytes expected) but returns `-1` — the conventional
"unknown/unavailable" sentinel value `URLConnection.getContentLength()`
returns when the underlying implementation can't determine a length. This
means CratonVM's WAR-URL-connection handling isn't populating or is
losing the content-length metadata for this nested-archive entry, even
though the entry itself is presumably still readable (the test would
likely have failed earlier/differently if the resource were missing
entirely).

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on). Verified via a fresh same-session HotSpot
run: PASSES on HotSpot.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName warurlconn `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.webresources.war.TestWarURLConnection
```

## Recommendation

Read `org.apache.catalina.webresources.war.WarURLConnection` (or wherever
Tomcat's nested-jar `war:...!/...` URL handler lives) and trace how
`getContentLength()` is meant to be populated — likely from a
`ZipEntry.getSize()`/`JarEntry` lookup during `connect()`. Check whether
CratonVM's `java.util.zip`/`java.util.jar` real-mode implementation
returns `-1` for entry size in this specific nested-archive-within-archive
access pattern (a `.war` containing further packaged resources) even
though a plain single-level jar/zip entry size lookup might work fine
elsewhere — this smells like a nested-URL-scheme-specific gap rather than
a general zip/jar bug, given how narrowly scoped the failure is.
