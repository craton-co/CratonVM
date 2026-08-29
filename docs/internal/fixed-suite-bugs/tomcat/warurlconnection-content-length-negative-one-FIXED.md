# TestWarURLConnection — getContentLength() returns -1 instead of the real size

**Superseded reopening report (2026-07-15).** The fix below (commit `09a505435`,
confirmed present in the exact binary tested — `war_nested_jar_bytes` is in
the built `native-builtins/src/net_phase_e.rs`) does not resolve the bug on
this Windows box: a fresh rerun on `dev` @ `f23a3f42a` reproduces the
**identical** original failure —
```
1) testContentLength(org.apache.catalina.webresources.war.TestWarURLConnection)
java.lang.AssertionError: expected:<137> but was:<-1>
```
Since the fix's own validation section below doesn't specify a platform and
the fix parses a `war:file:<war-path>*/WEB-INF/lib/test.jar` string to
locate the nested jar, a leading candidate is Windows path syntax
(`C:\...` drive prefix, `\` separators) breaking whatever prefix/pattern
match `war_nested_jar_bytes` uses to recognize the `war:file:<path>*/...`
shape — the fix may only have been validated on Linux. Needs a Windows-
specific repro/trace before assuming a plain regression.

## Closure verification (2026-07-15)

**Status: FIXED / stale reopen retired.** The original native fix remains on
`origin/dev` at `a9b838c4e`, and no later change touches its
`JarURLConnection` resolver. A fresh Windows release build from that exact
revision, with a dedicated worktree target, ran the supplied Tomcat fixture
four times: `TestWarURLConnection` passed three times with the default JIT and
once with `--nojit` (`OK (1 test)` every time). The reported
`expected:<137> but was:<-1>` result is not reproducible.

The former `TestHandlerIntegration.testToURI` annotation-scanning timeout is
not a `WarURLConnection`/content-length residual: HotSpot passes it in 8.88s,
while CratonVM still exceeds the focused 60s guard in
`DataInputStream.readUTF()` during deployment. It belongs to the separately
tracked embedded-server deployment throughput wall in
`docs/internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md`,
not this fixed URL-content contract.

**Original status:** FIXED. **Severity:** medium. **HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.catalina.webresources.war.TestWarURLConnection.testContentLength`
failed:
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
size (137 bytes expected) but returned `-1`.

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on). Verified via a fresh same-session HotSpot
run: PASSES on HotSpot.

## Root cause

The test URL is `jar:war:file:<war-path>*/WEB-INF/lib/test.jar!/META-INF/resources/index.html`
— a jar entry (`../../../../apps/META-INF/resources/index.html`) inside a jar
(`WEB-INF/lib/test.jar`) that is itself packaged inside a WAR, accessed via
Tomcat's `war:` pseudo-protocol (which uses `*/` as its own separator,
converted to `jar:...!/...` form by `UriUtil.warToJar`).

`WarURLConnection.getContentLength()` delegates straight to the wrapped
`innerJarUrl.openConnection().getContentLength()`. CratonVM's native
`java/net/JarURLConnection.getContentLength()`/`getJarEntry()`
(`jar_url_entry_size` / `jar_url_lookup_entry` in
`native-builtins/src/net_phase_e.rs`) parsed a `jar:[file:]<path>!/<entry>`
external form by opening `<path>` directly as an on-disk zip file. For this
URL, `<path>` was `war:file:<war-path>*/WEB-INF/lib/test.jar` — not a real
filesystem path — so `File::open` always failed and the lookup silently
returned `-1`. `getInputStream()`/`getEntryName()` were unaffected: they
either delegate to `URL.openStream()` (which already had a working
single-level `jar:file:...!/...` byte-read path once the `war:` layer
resolves down to it) or just parse the entry-name substring.

## Fix

Added `war_nested_jar_bytes()` (`net_phase_e.rs`): recognizes a
`war:file:<war-path>*/<entry-in-war>` jar-file component, reads the nested
jar's bytes out of the enclosing WAR's zip central directory (via the
existing `cached_nested_jar` byte cache), and returns them as an in-memory
archive to look the innermost entry up in. Wired into both
`jar_url_entry_size` (`getContentLength`/`getContentLengthLong`) and
`jar_url_lookup_entry` (`getJarEntry`), sharing a new
`jar_entry_value_from_archive` helper (generic over `Read + Seek`) so both
the plain-on-disk and WAR-nested paths build the returned `JarEntry`
identically.

`TestWarURLConnection.testContentLength`: `OK (1 test)`.

Regression-checked the rest of the jar/war/URL-stream-handler cluster
(all still pass, no timing regressions): `TestJarWarResourceSet`,
`TestJarResourceSet[Internal/Mount/MountTrailing]`, `TestJarContents`,
`TestJarInputStreamWrapper`, `TestTomcatURLStreamHandlerFactory`,
`TestClasspathUrlStreamHandler`, `TestResourceJars`,
`org.apache.tomcat.util.scan.TestAbstractInputStreamJar`,
`TestUriUtil{24,26,2A,40}`, `TestHandler`, `TestWebappClassLoader` (57s,
just slow — multiple embedded-Tomcat starts per test, not hung).

`org.apache.catalina.webresources.war.TestHandlerIntegration.testToURI`
(which deploys the same `war-url-connection.war` as a real webapp) is
**pre-existing, unrelated flakiness** — historical `.suite/results/*.csv`
runs from as far back as 2026-06-29/06-30/07-01 (weeks before this doc
existed) already show it alternating PASS (77-558s) / HANG (120-300s
timeout) on the same dev line, well before this fix. The observed hang (via
`--stack-dump-on-timeout`) is a single main thread stuck in
`BufferedInputStream.read()` inside `ContextConfig.processAnnotationsJar` →
BCEL `ClassParser` — a `getInputStream()`/annotation-scanning path this fix
does not touch (this fix only changes `getContentLength()`/`getJarEntry()`).
Not caused or fixed by this change; left as a separate, pre-existing,
load-dependent issue.

Fixed on branch `fix/warurlconnection-content-length-20260713`.
