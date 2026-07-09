# TestAccessLogValve / TestRewriteValve — connection-level `-1` response failures

**Status:** OPEN, needs re-verification. **Severity:** unclear pending
re-check. **HotSpot:** PASS on both.

## Summary

Two unrelated valve classes both fail with a response-code `-1`, which in
this codebase's HTTP test client convention indicates the connection
failed/reset before a status line was read (not a real HTTP response):

```
org.apache.catalina.valves.TestAccessLogValve
java.lang.AssertionError: expected:<200> but was:<-1>

1) testUtf8WithBothQsFlagsRBNE(org.apache.catalina.valves.rewrite.TestRewriteValve)
java.lang.AssertionError: expected:<400> but was:<-1>
```

**Caution:** this session has repeatedly established that a `-1`
response-code result is *often* (though not always) a parallel-load/
contention artifact rather than a genuine bug — the `-Parallel 4` re-check
round earlier in this investigation found that 27 of 30 apparent hangs at
low timeout were pure contention noise, and `-1`-style connection failures
have shown similar false-positive behavior under host load in this
codebase's history. These two classes have **not yet been re-verified in
isolation** (`-Parallel 1`, generous timeout) to confirm they are genuine
rather than artifacts of the shard's concurrent load at capture time.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, `-Parallel 4`/`-Parallel 2` shards, dev commit range
`33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName valveconn `
  -Start <idx> -Count 1 -TimeoutSec 120 -Parallel 1
# org.apache.catalina.valves.TestAccessLogValve
# org.apache.catalina.valves.rewrite.TestRewriteValve
```

## Recommendation

**Before treating as confirmed bugs**, re-run both classes individually with
`-Parallel 1` on an otherwise-idle host to rule out contention. If they
still reproduce in isolation: `TestAccessLogValve`'s failure is likely
unrelated to logging itself and more about the underlying request/response
cycle failing outright (check server-side logs for the actual connection-
reset cause). `TestRewriteValve.testUtf8WithBothQsFlagsRBNE` involves UTF-8
query-string handling with "both QS flags" and `RBNE` (likely
"Redirect-Because-No-Escape" or similar rewrite-rule flag combination per
Tomcat's `RewriteValve` flag vocabulary) — if genuine, check CratonVM's
query-string UTF-8 decode path under that specific rewrite-flag combination,
possibly related to the broader URI-decode gap family
(`reference_par_classpath_extension_uri_decode`).

## 2026-07-09 worker isolation

No isolated `TestAccessLogValve` / `TestRewriteValve` rerun was possible in
this worktree because `apps/tomcat-suite-runner` is absent. The native
`SocketChannel.close()` change made for the sibling
`TestSwallowAbortedUploads` note addresses an abortive Windows close path that
can produce connection-level failures, but it does not prove these two `-1`
responses are genuine.

Current classification after this worker: still "needs re-verification".
Treat as contention/noise-prone until each class reproduces under `-Parallel 1`
with a generous timeout and server-side logs showing the close/reset source.
