# TestHttp2Section_5_1 — max-active-streams / waiting-stream RST behavior

**Status:** FIXED / retired on 2026-07-14. **Severity:** medium (HTTP/2
protocol-conformance edge).

## Resolution

The three residual parameterizations recorded in the original note are no
longer reproducible. A fresh real-JDK release build passes the complete
`org.apache.coyote.http2.TestHttp2Section_5_1` class: **26/26**.

The current-head build initially exposed a separate bootstrap regression before
any HTTP/2 test could start: `ManagementFactory.getPlatformMBeanServer()`
returned a synthetic `MBeanServer` while its abstract interface methods were
still filtered as synthetic stubs. Tomcat then failed JMX registration before
opening a connection. The fix registers the platform-server fallback and its
`MBeanServer` operations as real-JDK bridge methods, and forces dispatch of
the synthetic interface receiver to those bridges.

This restores normal Tomcat startup and leaves the concrete HTTP/2 stream-state
implementation to run unchanged. The former residuals all pass:

* `testExceedMaxActiveStreams01` with synchronous and asynchronous I/O;
* `testErrorOnWaitingStream02` with asynchronous I/O; and
* every other parameterization in the section 5.1 class.

## Verification

On Azure, using a fresh uniquely named release binary built from this change:

```bash
cd /data/data/apps/tomcat
CP=$(cat .suite/cp-linux-fixed.txt)
/data/cratonvm-http2-s51-maxactive-residuals-20260714/cratonvm-http2s51-jmxbridge-20260714 \
  --java-home /home/victor/jdk25 -Xmx2g \
  -Dtomcat.test.basedir=/data/data/apps/tomcat/output/build \
  -Dtomcat.test.temp=/data/cratonvm-http2-s51-maxactive-residuals-20260714/tomcat-tmp-jmxbridge \
  -cp "$CP" org.junit.runner.JUnitCore org.apache.coyote.http2.TestHttp2Section_5_1
```

Result: `OK (26 tests)` in 17.737 seconds.

The focused MBeanServer probe also completed normally and its native-registry
census contained `javax/management/MBeanServer.isRegistered`, proving that the
bridge used by Tomcat is retained in real-JDK mode.
