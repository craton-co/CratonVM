# `TestManagerWebapp` — `ExpandWar` file copy fails with `seek0: bad fd for rw_seek`

> ## ✅ 2026-08-03 FIXED — retired from `docs/known-issues/tomcat/`
>
> **Two defects, both fixed. The second was invisible until the first was.**
>
> ### Defect 1 — the `seek0` this doc is about
>
> `FdTable::rw_seek` (`native-api/src/fd_table.rs`) accepted only
> `FileEntry::FileReadWrite`. But `open_read`/`open_write` register their fds as
> `FileRead(BufReader)` / `FileWrite(BufWriter)`, so every `FileChannel` obtained
> from a `FileInputStream` or `FileOutputStream` had a `seek0` that could only
> fail.
>
> On Windows that is not an edge case. `FileDispatcherImpl
> .transferToDirectlyNeedsPositionLock()` is `true`, so
> `FileChannelImpl.transferToDirect` brackets every transfer with
> `synchronized (positionLock) { long pos = position(); … position(pos); }`, and
> `position()` is `nd.seek(fd, -1)`. `ExpandWar.copy` is exactly
> `fis.getChannel().transferTo(pos, size, fos.getChannel())`, so the first file
> it staged threw. The two `SocketTimeoutException`s this doc recorded were the
> downstream broken-webapp symptom, not independent defects.
>
> The doc's suspicion of a reused/closed handle or a concurrent-close race was
> wrong — no race, no lifetime bug: the entry was present and healthy, `rw_seek`
> simply had no arm for its variant. The lower-cased drive letter the doc
> flagged as a possible clue is unrelated (it is just the runner's cwd casing).
>
> `pread_at`, `pwrite_at` and `clone_file` had already been extended to the
> buffered variants one call site at a time; the seek/position/size/truncate/sync
> family had not. Fixed by converting the idiom rather than the one site: a
> private `with_seekable()` dispatches over all three file variants and
> `rw_seek`/`rw_position` route through it, with the matching gaps filled in
> `rw_read`, `rw_write`, `file_size`, `rw_set_length` and `rw_sync`. Buffered
> entries are seeked through the wrapper, never the file behind it —
> `BufReader`'s `Seek` reconciles its read-ahead buffer, `BufWriter`'s flushes
> pending bytes at their original offset first. `rw_sync` grew a `FileWrite` arm
> only: `FlushFileBuffers` on a read-only Windows handle is
> `ERROR_ACCESS_DENIED`, so accepting a read fd would mean lying on one platform
> and erroring on the other. Commit `a6e91d445`.
>
> ### Defect 2 — `MonitorInfo` wrote its getter names, not its field names
>
> With the copy working, `testServlets` still failed `expected:<200> but
> was:<500>`: `GET /manager/text/threaddump` threw
>
> ```
> NullPointerException: Cannot invoke "java.lang.StackTraceElement.toString()"
> because the return value of "java.lang.management.MonitorInfo.getLockedStackFrame()" is null
>   at org.apache.tomcat.util.Diagnostics.getThreadDump(Diagnostics.java:335)
> ```
>
> `alloc_jmx_monitor_info` (`native-builtins/src/jmx.rs`) set `lockedStackDepth`
> and `lockedStackFrame`. Those are the *getter* names. `javap -p
> java.lang.management.MonitorInfo` on JDK 25:
>
> ```
> private int stackDepth;
> private java.lang.StackTraceElement stackFrame;
> ```
>
> `set_field_by_name` on a name the class does not have is a silent no-op, so
> both writes went nowhere and **every `MonitorInfo` this VM ever produced**
> reported `getLockedStackFrame() == null` and `getLockedStackDepth() == 0`.
> Only these two were wrong — `className`/`identityHashCode` are inherited from
> `LockInfo` and really are spelled that way, which is why the object looked
> healthy. Depth 0 with a null frame is a state the JDK cannot produce, and
> `Diagnostics` depends on that: it stores the monitor at
> `monitorDepths[getLockedStackDepth()]` and then calls
> `getLockedStackFrame().toString()`.
>
> Fixed by writing the real names and deriving depth and frame together from the
> same array read, re-read from the pinned stack-trace array on every iteration
> (the previous code hoisted one `ObjectRef` above a loop whose body allocates).
> A thread with no stack frames now reports no locked monitors, which is what
> HotSpot produces structurally — its dumper discovers monitors while walking
> Java frames. Commit `aacb72415`.
>
> ### Evidence
>
> Two new standalone probes, both green on HotSpot 25.0.3.9 first:
>
> | probe | HotSpot | CratonVM before | CratonVM after |
> |---|---|---|---|
> | `probes/Seek0TransferProbe.java` | 13/13 PASS | 6 FAIL (`seek0: bad fd for rw_seek` on both channel `position()`s, both `transferTo` loops, `transferFrom`) | 13/13 PASS |
> | `probes/ThreadDumpMonitorInfoProbe.java` | 19/19 PASS | 11 FAIL, every monitor `depth=0 frame=null`, `Diagnostics` NPE | ALL OK, 10 monitors checked |
>
> The `seek0` baseline was reproduced on two independently built binaries
> (`cratonvm-wsjsse-base-20260803.exe` and a fresh `origin/dev` build) plus
> end-to-end in the real class. Six `fd_table` unit tests were added for the new
> arms; all 60 in that module pass.
>
> `TestManagerWebapp` went 0/3 → 1/3 (seek0 fixed) → **2/3** (MonitorInfo
> fixed). HotSpot control fresh-verified the same day: `OK (3 tests)` in 12.8 s.
>
> ### Residual — tracked separately, NOT this doc's defect
>
> `testBug57700` and `testDeploy` still fail, on timing alone, because a webapp
> deploy's BCEL annotation scan runs 226x slower than HotSpot and is never
> JIT-compiled (`--nojit` costs the same; zero `<init>` methods compile in a
> constructor-dominated workload). That is the wall docs 04/29/30/31 have owned
> since July, and it is the same residual
> `managerwebapp-deploy-bare-assertion-FIXED.md` retired against in July naming
> these same two methods. Re-measured from scratch today and written up with
> fresh numbers and standalone probes in
> `../../../known-issues/tomcat/!webapp-deploy-annotation-scan-interpreted-226x.md`.
> Per the known-issues triage rule this doc retires, since its own defect is
> fixed and the residual has a separate open owner.

---

## Original report (as the doc stood in `known-issues`)

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
