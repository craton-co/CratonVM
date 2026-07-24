# Missing `httpd` binary — 8 integration/proxy classes

> ✅ **FULLY RESOLVED, 2026-07-23.** `sudo apt-get install -y apache2`
> (Debian names the binary `apache2`, not `httpd`) then `sudo ln -sf
> /usr/sbin/apache2 /usr/local/bin/httpd` (the test only looks for a literal
> `httpd` on `PATH` unless `-Dtomcat.test.httpd.path` is set). **No further
> mod_proxy/config work was needed** — all 8 classes below passed on both
> HotSpot and CratonVM immediately after the symlink, with zero CratonVM
> regressions found in this category. This is the one fully-clean fix among
> the 6 in this folder.

**Not a CratonVM bug** — confirmed, all 8 classes now pass on both VMs.

## Symptom

```
java.io.FileNotFoundException: test/org/apache/tomcat/integration/httpd/httpd-binary.lock (No such file or directory)
	at org.apache.tomcat.integration.httpd.HttpdIntegrationBaseTest.obtainHttpdBinaryLock(HttpdIntegrationBaseTest.java:86)
```

## Affected classes

- `org.apache.tomcat.integration.httpd.TestBasicProxy`
- `org.apache.tomcat.integration.httpd.TestChunkedTransferEncodingWithProxy`
- `org.apache.tomcat.integration.httpd.TestFullReverseProxy`
- `org.apache.tomcat.integration.httpd.TestLargePayloadWithProxy`
- `org.apache.tomcat.integration.httpd.TestRemoteIpValveWithProxy`
- `org.apache.tomcat.integration.httpd.TestSSLValveWithProxy01`
- `org.apache.tomcat.integration.httpd.TestSSLValveWithProxy02`
- `org.apache.tomcat.integration.httpd.TestSessionWithProxy`

## Root cause

These tests proxy real HTTP traffic through Apache `httpd` + `mod_proxy`/
`mod_jk` in front of the embedded Tomcat instance under test. `httpd` isn't
installed on this Azure host, so `HttpdIntegrationBaseTest` can't even
acquire its startup lock file.

## Fix

```sh
sudo apt-get install -y apache2 libapache2-mod-jk   # or the local distro equivalent
```

Then either point `-Dtest.httpd.path` at the installed binary (Ant's
`test-httpd-exists` target auto-detects `httpd` on `PATH` or via
`test.httpd.path`) or export it as an env override in
`run-tomcat-suite.sh`. Check `test/org/apache/tomcat/integration/httpd/*.conf`
templates for what mod_proxy config each test expects — some (SSLValve*)
also need a working TLS cert on the httpd side.

## Verify

Run `test-httpd-exists` (or just `which httpd`) before the suite; if it
resolves, rerun these 8 classes under HotSpot first to confirm the fixture
gap is actually closed before comparing against CratonVM.
