# 9 CratonVM regressions revealed by completing the 6 fixture-gap fixes (2026-07-23)

Implemented all 6 fixture-completion items from this folder (see each doc's
own "RESOLVED" note for what was actually done — several of the original
root-cause theories were wrong; corrected in place). Reran the 23 affected
classes under both HotSpot and CratonVM (`--Xmx 8g` uniformly, real JDK 25,
same fixture: `/data/data/apps/tomcat`).

## Result: 12 PASS on both / 9 confirmed CratonVM-only regressions / 2 still fail on both (different, narrower reasons than before)

| Class | HotSpot | CratonVM | Notes |
|---|---|---|---|
| `org.apache.catalina.ant.TestDeployTask` | PASS | FAIL | see below — real bug found |
| `org.apache.catalina.manager.TestManagerWebapp` | PASS | FAIL | `SocketTimeoutException` + one `500` |
| `org.apache.catalina.manager.TestManagerWebappSsl` | PASS | FAIL | not individually triaged |
| `org.apache.catalina.mapper.TestMapperWebapps` | PASS | FAIL | not individually triaged |
| `org.apache.catalina.servlets.TestDefaultServlet` | PASS | FAIL | `expected:<200> but was:<500>` |
| `org.apache.catalina.servlets.TestWebdavServlet` | PASS | FAIL | not individually triaged |
| `org.apache.tomcat.util.net.TestSsl` | PASS | **HANG** (300s timeout) | not individually triaged |
| `org.apache.tomcat.util.buf.TestByteChunkLargeHeap` | PASS (8g) | FAIL | not individually triaged — may still need more than 8g on the CratonVM side specifically |
| `org.apache.tomcat.util.buf.TestCharChunkLargeHeap` | PASS (8g) | FAIL | not individually triaged |

## One regression root-caused: `%20`-encoded path not decoded back to a space

`TestDeployTask.bug58086a` fails only on CratonVM:

```
org.apache.tools.ant.BuildException: java.io.IOException: URL.openStream: read jar
/data/data/tomcat-dohead-fixture-20260717/test/deployment/dir%20with%20spaces/context.jar:
No such file or directory (os error 2)
```

The directory **does exist on disk** with a literal space
(`test/deployment/dir with spaces/`) — confirmed via `ls`. HotSpot's
`URL.openStream()`/`URLConnection` correctly percent-decodes `%20` back to a
space when resolving the `file:` URL to a real path; CratonVM's
implementation of whatever native/synthetic URL-to-file resolution backs
`org.apache.tools.ant.DeployTask`'s jar-reading path does not, and looks for
a literal `dir%20with%20spaces` directory that doesn't exist. This is a
clean, well-isolated, reproducible bug — good candidate to fix first among
the 9.

## Two classes still fail on both VMs — but with DIFFERENT, narrower symptoms than before

Fixture completion didn't fully close these two; the remaining gap is
smaller and different in character from what was originally documented:

- **`org.apache.catalina.tribes.group.interceptors.TestEncryptInterceptorLargeHeap`**
  — HotSpot: a semantic assertion failure (`actual array was null`) even at
  `-Xmx8g`, not an OOM anymore — the huge-payload roundtrip itself doesn't
  work correctly at this heap size on HotSpot either; needs more heap or a
  closer read of what heap size upstream Ant's `test.xmx` actually uses for
  this class. **CratonVM: a hard abort**, not a graceful test failure:
  `FATAL: OutOfMemoryError: young gen exhausted — tried to allocate
  1073741888 bytes, from-space has 1074300032/2147483648 used` — i.e. even
  at `--Xmx 8g`, a single ~1GB allocation blows the young generation.
  Matches the known, previously-tracked "true-OOM still aborts" gap (young
  gen doesn't grow/promote to accommodate one huge object) rather than
  gracefully throwing `OutOfMemoryError` back to Java code the way HotSpot
  does. Worth cross-referencing with any existing GC/humongous-allocation
  known-issue docs before opening a new one.
- **`org.apache.catalina.loader.TestVirtualContext.testVirtualClassLoader`**
  — was `expected:<200> but was:<404>` before the fixture fix (fixed by
  creating `test/webapp-virtual-webapp/target/classes/rsrc/` +
  `test/webapp-virtual-library/target/WEB-INF/classes/`, see this folder's
  `unbuilt-virtual-webapp-submodule.md`). Now: HotSpot still 404s (this
  specific method needs something else the placeholder directories didn't
  provide), and **CratonVM now returns a 500** where HotSpot returns 404 —
  a smaller, different regression than the original.

## What was actually done to the fixture (host-local, not git-tracked)

On the Azure host, in `/data/data/apps/tomcat` (`/data/data/tomcat-dohead-fixture-20260717`):

1. Appended `/usr/share/java/ant.jar` and `/usr/share/java/ant-launcher.jar`
   to `.suite/cp-linux-fixed.txt`.
2. `sudo apt-get install -y apache2` + `sudo ln -sf /usr/sbin/apache2
   /usr/local/bin/httpd` (Debian ships `apache2`, not `httpd`, on `PATH` —
   the test only looks for `httpd` unless `-Dtomcat.test.httpd.path` is
   set). No further mod_proxy/config work was needed — all 8 httpd tests
   passed on both VMs immediately after the symlink.
3. Ran `ant deploy` (JAVA_HOME=`/home/victor/jdk25`) in the fixture root —
   this alone populated `output/build/lib/*.jar` (the actual missing piece
   behind most of the "manager/mapper/defaultservlet" 404s and
   `LifecycleException`s, not the `conf/Catalina/localhost/*.xml` files this
   folder's `missing-catalina-localhost-context-configs.md` originally
   guessed — that theory was wrong, see the doc's own correction note).
4. Created `test/webapp-virtual-webapp/target/classes/rsrc/resourceX.properties`
   (placeholder) and `test/webapp-virtual-library/target/WEB-INF/classes/`
   (empty dir) — no Maven build was actually needed (there's no `pom.xml`;
   `unbuilt-virtual-webapp-submodule.md`'s theory was also wrong).
5. Ran the 23-class rerun with `MAX_HEAP=8g` uniformly (not just for the 3
   `*LargeHeap` classes) for simplicity.

None of this touches the CratonVM git repo — it's all fixture state on the
Azure host. The actual code fix for the `%20` bug (and whichever of the
other 8 turn out to need real CratonVM changes) is separate follow-up work.
