# `TestManagerWebapp` — read-timeout residual behind the fixed `seek0` bug

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | medium |
| **Discovered** | 2026-08-06, complete 651-class Tomcat suite rerun |
| **Prior history** | `fixed-suite-bugs/tomcat/testmanagerwebapp-expandwar-seek0-bad-fd-FIXED.md` — the original `ExpandWar` `seek0: bad fd for rw_seek` defect is confirmed fixed in this build (no longer appears); its retirement commit (`d77025185`) says it "opens the deploy-scan wall behind it" but no doc for that residual was found in either `docs/known-issues` or `docs/internal` |

## Symptom

With the `seek0` bug fixed, `TestManagerWebapp` still fails 2 of 3 methods,
now with a plain client-side read timeout instead of the old `ExpandWar`
error:

```
1) testBug57700(org.apache.catalina.manager.TestManagerWebapp)
java.net.SocketTimeoutException: Read timed out
	at sun.nio.ch.NioSocketImpl.timedRead(NioSocketImpl.java:277)
	at org.apache.catalina.startup.SimpleHttpClient.readResponse(SimpleHttpClient.java:273)
	at org.apache.catalina.manager.TestManagerWebapp.testBug57700(TestManagerWebapp.java:571)
```

Total class time is 177s (vs. HotSpot's low-teens-of-seconds for this class
historically) — consistent with a slow deploy/scan cycle that occasionally
exceeds the client's read timeout rather than a hard hang, similar in shape
to the general deploy-throughput-wall family
(see [gc-moving-young-persistent-nonmoving-fallback-regression.md](gc-moving-young-persistent-nonmoving-fallback-regression.md)),
though not yet confirmed to share that exact cause.

## Not yet done

- HotSpot control run on this exact build/fixture (the class historically
  passes on HotSpot, but not re-verified this round).
- Standalone isolation to rule out shared-host timing sensitivity, given the
  `SimpleHttpClient` read timeout is a fixed wall-clock value that a slow
  deploy could plausibly exceed even without a real defect.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.catalina.manager.TestManagerWebapp
```
