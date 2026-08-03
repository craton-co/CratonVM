# `TestManagerWebapp` — `ExpandWar` file copy fails with `seek0: bad fd for rw_seek`

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | high — cascades into all 3 test methods in the class |
| **HotSpot** | PASS (3/3, `apps/tomcat`, `-Xmx2g`, fresh-verified 2026-08-03) |
| **CratonVM** | FAIL (0/3) |
| **Discovered** | 2026-08-03, rerunning the 07-31 4-shard FAIL/HANG set after merging `dev` (`c1fe51a24`) |

## Symptom

`org.apache.catalina.manager.TestManagerWebapp` fails all 3 of its test
methods (`testBug57700`, `testDeploy`, `testServlets`). The first visible
error is during `ExpandWar`'s file copy, while it stages a fresh copy of the
`examples` webapp into the test's temp `webapps` dir:

```
ERROR [org.apache.catalina.startup.ExpandWar] Error copying
[C:\craton\CratonVM\apps\tomcat\output\build\webapps\examples\index.html] to
[C:\craton\cratonvm\apps\tomcat\output\test-tmp\test17856213308433926425\webapps\examples\index.html]
(java/io/IOException: seek0: bad fd for rw_seek)
```

Note the destination path's drive-letter segment is lower-cased
(`C:\craton\cratonvm\...`) versus the source's `C:\craton\CratonVM\...` —
Windows paths are case-insensitive so this alone should not break the copy,
but it may be a symptom of the same underlying path/handle-table
normalization that produces the bad fd; not yet confirmed as causal.

After the copy fails, the class's other two failures are consistent with a
resulting broken/undeployed webapp rather than independent defects:

```
1) testBug57700(org.apache.catalina.manager.TestManagerWebapp)
java.net.SocketTimeoutException: Read timed out
	at sun.nio.ch.NioSocketImpl.timedRead(NioSocketImpl.java:277)
	at org.apache.catalina.startup.SimpleHttpClient.readResponse(SimpleHttpClient.java:273)
	at org.apache.catalina.manager.TestManagerWebapp.testBug57700(TestManagerWebapp.java:571)
```

`testDeploy` and `testServlets` fail similarly (`E.E` in the JUnit dot output
after the `ExpandWar` error).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1; $env:CRATONVM_ROOTSNAP_CACHE=1
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.catalina.manager.TestManagerWebapp
```

HotSpot control (same classpath, same cwd): `OK (3 tests)` in 12.9s.

## Suspected root cause (not yet isolated)

`seek0: bad fd for rw_seek` is a native-io error string, not a Java-level
exception message — this points at CratonVM's `RandomAccessFile`/`FileChannel`
native seek implementation returning an invalid/stale file descriptor during
`ExpandWar`'s byte-copy loop (open → seek → read/write → close). Candidates,
not yet checked against the source:

- A file-handle table entry reused/closed between open and seek (matches the
  general pattern in [[reference_close_must_drop_the_last_handle_try_clone_keeps_a_listener_alive]]).
- The copy racing a concurrent close of the same underlying fd from another
  thread (JUnit's `TestWatcher`/teardown machinery, or a prior test's leftover
  temp-dir cleanup).

Not yet bisected against `dev` history; no known-issue doc previously covered
this exact `seek0` signature (checked `docs/internal/fixed-suite-bugs` and
`docs/known-issues` — no hits).
