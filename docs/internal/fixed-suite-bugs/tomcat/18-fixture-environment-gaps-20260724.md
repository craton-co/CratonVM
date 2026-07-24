# Group 18 — Tomcat Linux fixture environment gaps (2026-07-24)

Not CratonVM bugs. These 35 classes fail identically on **both** CratonVM
and real JDK 25 (HotSpot) in the Linux test fixture
(`/data/data/tomcat-dohead-fixture-20260717`, symlinked
`/data/data/apps/tomcat`), after correcting for the CWD harness bug described
in [16](16-full-suite-6shard-rerun-20260721.md)'s 2026-07-24 correction
addendum. Categorized by actual root cause (grep'd from each class's log, not
guessed from the class name) so a future session completing the fixture can
knock out several classes per fix instead of chasing each individually.

Source data: `apps/tomcat-suite-runner/RESULTS-20260724-cwdfix.md`,
`.suite/results/{cwdfix-craton-20260724,cwdfix-hotspot-20260724}/` on the
Azure host.

## A. Missing `org.apache.tools.ant` on the classpath (2 classes)

`NoClassDefFoundError: org/apache/tools/ant/Task`. These two classes drive
Ant's own `Task`/`Copy`/etc. APIs directly (`TestDeployTask` exercises
Tomcat's `DeployTask` Ant task; `TestJspC` drives the JSP-to-servlet
precompiler, which is itself implemented as an Ant task,
`org.apache.jasper.JspC` extends `org.apache.tools.ant.Task`). The suite's
`.suite/cp-linux-fixed.txt` classpath has no `ant.jar`/`ant-launcher.jar`.
**Fix:** add Ant's own jars (already present at
`.suite/apache-ant/lib/ant.jar` on the Windows harness, or installable via
`apt-get install ant`) to `cp-linux-fixed.txt`.

- `org.apache.catalina.ant.TestDeployTask`
- `org.apache.jasper.TestJspC`

## B. Missing `httpd` binary (8 classes)

`FileNotFoundException: test/.../httpd-binary.lock` from
`HttpdIntegrationBaseTest.obtainHttpdBinaryLock()` — these tests proxy
through a real Apache `httpd` + `mod_proxy`/`mod_jk`, which isn't installed
on this host. **Fix:** `sudo apt-get install apache2` (or point
`-Dtest.httpd.path` at a built one) and set up the mod_proxy config these
tests expect (see `test/org/apache/tomcat/integration/httpd/`).

- `org.apache.tomcat.integration.httpd.TestBasicProxy`
- `org.apache.tomcat.integration.httpd.TestChunkedTransferEncodingWithProxy`
- `org.apache.tomcat.integration.httpd.TestFullReverseProxy`
- `org.apache.tomcat.integration.httpd.TestLargePayloadWithProxy`
- `org.apache.tomcat.integration.httpd.TestRemoteIpValveWithProxy`
- `org.apache.tomcat.integration.httpd.TestSSLValveWithProxy01`
- `org.apache.tomcat.integration.httpd.TestSSLValveWithProxy02`
- `org.apache.tomcat.integration.httpd.TestSessionWithProxy`

## C. `*LargeHeap` classes OOM at the flat `-Xmx2g` (3 classes)

`OutOfMemoryError: Java heap space`. Ant's own `<test>` fileset selector
(`build.xml`) only *includes* `**/*LargeHeap.java` when
`test.includeLargeHeap` is set, and the real Ant run applies a bigger
per-class `-Xmx` override for them (`test.xmx`) that this harness's flat
`-Xmx2g`/`--Xmx 2g` doesn't replicate. **Fix:** either exclude
`**/*LargeHeap.java` from the class list by default (matching Ant's own
default), or give this specific harness a per-class heap override (e.g.
`MAX_HEAP=6g` for any class matching `*LargeHeap`).

- `org.apache.catalina.tribes.group.interceptors.TestEncryptInterceptorLargeHeap`
- `org.apache.tomcat.util.buf.TestByteChunkLargeHeap`
- `org.apache.tomcat.util.buf.TestCharChunkLargeHeap`

## D. Missing `conf/Catalina/localhost/*.xml` per-webapp context configs (8 classes)

`conf/Catalina/localhost/` doesn't exist in this fixture at all (verified —
`ls` on it 404s). Ant's `deploy` target's real `<copy>` also stages
per-webapp context XML fragments there (`manager.xml`, `host-manager.xml` —
they restrict `Valve`/`RemoteAddrValve` access and set docBase/privileged
flags) that the earlier `conf/*.xml`-only fix in
[16](16-full-suite-6shard-rerun-20260721.md) didn't cover. Without them, the
`manager`/`host-manager` webapps either fail to start
(`LifecycleException: Failed to start component`) or serve **404** for
routes that depend on the context being registered as its own `Host`-scoped
context (mapper tests, `TestDefaultServlet`/`TestWebdavServlet`,
`TestSsl` — all show `expected:<200> but was:<404>` or an equivalent mapper
lookup miss). **Fix:** copy `conf/Catalina/localhost/{manager,host-manager}.xml`
from a real Ant `deploy` run (or hand-author minimal equivalents) into
`output/build/conf/Catalina/localhost/`.

- `org.apache.catalina.manager.TestHostManagerWebapp`
- `org.apache.catalina.manager.TestManagerWebapp`
- `org.apache.catalina.manager.TestManagerWebappSsl`
- `org.apache.catalina.mapper.TestMapperListener`
- `org.apache.catalina.mapper.TestMapperWebapps`
- `org.apache.catalina.servlets.TestDefaultServlet`
- `org.apache.catalina.servlets.TestWebdavServlet`
- `org.apache.tomcat.util.net.TestSsl`

## E. Missing `output/build/lib/*.jar` (at least 1 class)

`output/build/lib/` doesn't exist at all in this fixture (Ant's `deploy`
target also populates this with `tomcat-util.jar` and friends via
`build-tomcat-jdbc`/`package`, neither of which ran here — only
`test-compile` did). **Fix:** run the real `package`/`deploy` Ant targets, or
manually stage the handful of jars `output/build/lib/` needs.

- `org.apache.catalina.startup.TestTomcatNoServer`

## F. Missing pre-built Maven test-webapp submodule (1 class)

`IllegalArgumentException: Unable to create WebResourceSet from
[.../test/webapp-virtual-webapp/target/classes]` — `target/classes` is a
Maven build-output convention; this test webapp submodule was never `mvn
compile`d in this fixture.

- `org.apache.catalina.loader.TestVirtualContext`

## G/H. Individually odd, not yet root-caused (2 classes)

- `org.apache.catalina.startup.TestTomcat` — `LifecycleException:
  Deliberately Broken` / `Deliberately Broken`. This looks like it could be
  the test **exercising** an intentionally-broken-webapp scenario rather
  than a real fixture defect — worth 5 minutes reading
  `TestTomcat.java`'s failing method before assuming it needs fixture work.
- `org.apache.jasper.compiler.TestNonstandardTagPerformance` — `Could not
  find class [org.apache.jasper.compiler.TestNonstandardTagPerformance]`, a
  self-referential `ClassNotFoundException` for its own class. Smells like a
  classloader/classpath-ordering quirk specific to this one class, not
  (yet) understood.

## I. Real CratonVM bug that happens to ALSO fail on HotSpot for an unrelated reason (1 class)

`org.apache.catalina.nonblocking.TestNonBlockingAPI` — HotSpot FAILs this
class too (a plain JUnit assertion failure, `expected:<200>` family — an
unrelated, not-yet-triaged issue, possibly fixture/timing), which is why the
HotSpot-pass/CratonVM-fail diff buckets it here rather than in the confirmed-
regressions list. **But CratonVM's failure mode is a genuine, isolated Rust
panic**, independent of whatever HotSpot's failure is:

```
thread 'http-nio-127.0.0.1-auto-22-exec-3' panicked at vm/src/runtime/value_stack.rs:237:25:
index out of bounds: the len is 24 but the index is 18446744073709551615
```

(`18446744073709551615` = `u64::MAX`, a `0usize - 1` underflow.) Fires on a
background NIO worker thread inside `LinkedBlockingQueue.take()` during
connector pause/stop teardown. The identical panic also reproduced in
`org.apache.tomcat.websocket.TestWebSocketFrameClientSSL` (see
[16](16-full-suite-6shard-rerun-20260721.md)'s confirmed-regressions list —
that class HotSpot-passes cleanly, so it's unambiguously a real regression).
**Treat this as a real, high-priority CratonVM VM bug** despite living in
the "both fail" bucket — don't let the HotSpot-also-fails classification
hide it.

## J. Contention artifacts, CONFIRMED — HANG was fake, but the underlying regressions were real; now FIXED (9 classes)

The 2026-07-24 HotSpot control pass ran while this shared Azure host's
`uptime` load average spiked to **70 → 148** (other concurrent sessions, not
this run) — see [[feedback_shared_host_multitenant_confound]]. These 9
classes were classified `HANG` (hit the 300s per-class timeout) on the
HotSpot side.

**Resolved 2026-07-23** (full writeup:
[hang-classification-unconfirmed-host-contention-FIXED.md](hang-classification-unconfirmed-host-contention-FIXED.md)).
Reran on a quiet host: HotSpot passed all 9 cleanly (confirming the HANG was
indeed the suspected contention artifact), but CratonVM failed all 9 with
real, deterministic errors — 8 from a shared `Hashtable.size()` field-index
collision breaking ecj JSP compilation (same bug fixed elsewhere via commits
`744401af4`/`b854dc01f`), and 1 (`TestCustomSsl`) from a new bug,
`HttpsURLConnection.setDefaultSSLSocketFactory` not unwrapping a delegating
`SSLSocketFactory` subclass. Both fixed; all 9 classes now PASS cleanly on
CratonVM on a quiet host, matching HotSpot:

- `jakarta.el.TestCompositeELResolver`
- `jakarta.el.TestOptionalELResolverInJsp`
- `jakarta.servlet.TestSessionCookieConfig`
- `jakarta.servlet.jsp.TestPageContext`
- `jakarta.servlet.jsp.el.TestImportELResolver`
- `org.apache.catalina.authenticator.TestFormAuthenticatorA`
- `org.apache.catalina.authenticator.TestFormAuthenticatorB`
- `org.apache.catalina.authenticator.TestFormAuthenticatorC`
- `org.apache.tomcat.util.net.TestCustomSsl`

## Summary table

| Category | Classes | Fixable? |
|---|---:|---|
| A — missing `ant.jar` | 2 | Yes, trivial (add jar to classpath) |
| B — missing `httpd` binary | 8 | Yes (`apt-get install apache2` + config) |
| C — `*LargeHeap` flat-heap OOM | 3 | Yes, trivial (exclude or bump `-Xmx`) |
| D — missing `conf/Catalina/localhost/*.xml` | 8 | Yes (copy from a real `ant deploy`) |
| E — missing `output/build/lib/*.jar` | 1 | Yes (`ant package`/`deploy`) |
| F — unbuilt Maven test-webapp submodule | 1 | Yes (`mvn compile` the submodule) |
| G/H — not yet root-caused | 2 | Needs individual investigation |
| I — real CratonVM panic (masked by unrelated HotSpot fail) | 1 | **Real VM bug — prioritize** |
| J — contention artifact (HANG) + 2 real bugs, ✅ FIXED 2026-07-23 | 9 | Done — see linked doc |

Categories A/B/C/D/E/F (23 classes) are all completable fixture work — doing
it would very likely turn several into either clean PASSes on both VMs or
newly-surfaced real CratonVM regressions (as happened when the CWD bug fix
alone flipped ~65 previously-miscategorized classes into confirmed
regressions — see [16](16-full-suite-6shard-rerun-20260721.md)). Worth
finishing before the next full-suite run.
