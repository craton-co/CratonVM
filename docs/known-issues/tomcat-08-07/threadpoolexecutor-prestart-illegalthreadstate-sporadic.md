# Sporadic `IllegalThreadStateException` from `Thread.start()` in Tomcat endpoint executor prestart

**Status: OPEN (low-rate, sporadic).** Found 2026-07-13 during the 64-class
`TestHttpServletDoHead*` family validation sweep; not DoHead-specific.

## Symptom

Very occasionally (≈1 in ~5,000 embedded-Tomcat starts on a loaded Windows
box; 3 occurrences in an 18,432-test sweep plus 1 in a follow-up solo run),
`tomcat.start()` fails during test `setUp` with:

```
org.apache.catalina.LifecycleException: Protocol handler start failed
	at org.apache.catalina.connector.Connector.startInternal(Connector.java:1310)
Caused by: java.lang.IllegalThreadStateException
	at org.apache.tomcat.util.threads.ThreadPoolExecutor.addWorker(ThreadPoolExecutor.java:756)
	at org.apache.tomcat.util.threads.ThreadPoolExecutor.prestartAllCoreThreads(ThreadPoolExecutor.java:1390)
	at org.apache.tomcat.util.threads.ThreadPoolExecutor.<init>(ThreadPoolExecutor.java:1089)
	at org.apache.tomcat.util.net.AbstractEndpoint.createExecutor(AbstractEndpoint.java:1927)
	at org.apache.tomcat.util.net.NioEndpoint.startInternal(NioEndpoint.java:369)
```

`ThreadPoolExecutor.addWorker:756` is `t.start()` on the worker thread it
just constructed — real `java.lang.Thread.start()` throwing
`IllegalThreadStateException` means the VM considered a **freshly
constructed, never-started thread** to be already started (threadStatus /
holder-state != NEW at start time). This is Tomcat's own
`org.apache.tomcat.util.threads.ThreadPoolExecutor` (plain app bytecode),
so no CratonVM executor natives are involved — the suspect surface is
CratonVM's `Thread` state tracking (`threadStatus` initialization /
visibility on the freshly-allocated Thread, or start()'s started-check
racing thread-object initialization), the same family as the
`Thread.getState()`-returns-non-NEW note in the project memory
(`reference_thread_getstate_new_bug`).

Observed at random parameterizations across different DoHead classes
(param 88, 117, 120, 140 — each parameterization boots its own Tomcat), on
both parallel-2 and solo runs, so it is not port churn / bind contention
(the failure is before bind matters, in executor prestart) and not tied to
any test's own logic.

## Repro

No deterministic repro. Statistical: any long embedded-Tomcat suite run;
each `TestHttpServletDoHead*` class boots Tomcat 288 times, so a 64-class
sweep (`apps/tomcat-suite-runner/run-tomcat-suite.ps1 -Start 28 -Count 64`)
gives ~18k starts and a few hits. Grep:

```
grep -l "IllegalThreadStateException" apps/tomcat/.suite/results/<run>/real-jit/*.log
```

paired with `Protocol handler start failed` in the same log.

## Impact

One test failure per hit (the affected parameterization's `setUp`); the
class otherwise completes. Under HotSpot this exception is impossible for
a just-constructed thread.
