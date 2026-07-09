# TestManagerWebapp — bare assertion failures in deploy/servlet-listing tests

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS.
**Related:** [Group 04 — embedded-server deployment throughput wall](../../internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md)
(OPEN, root-caused to `update_root_snapshot`) — this class also exercises
Tomcat Manager webapp deploy operations, so check there first before treating
this as an independent bug.

## Summary

`org.apache.catalina.manager.TestManagerWebapp` fails `testDeploy` and
`testServlets` with bare `AssertionError` (no message):
```
1) testDeploy(org.apache.catalina.manager.TestManagerWebapp)
java.lang.AssertionError
2) testServlets(org.apache.catalina.manager.TestManagerWebapp)
```
This class drives the Tomcat Manager webapp's deploy/list-servlets HTTP
endpoints. Given the sibling `TestHostConfigAutomaticDeployment*` cluster's
confirmed, still-open deploy-throughput wall (Group 04 doc), this failure
may be the SAME underlying deploy-path slowness/behavior gap manifesting as
a wrong response body/status here rather than an outright timeout — or it
may be an independent Manager-webapp-specific bug. Not yet distinguished.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName mgrwebapp `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.manager.TestManagerWebapp
```

## Recommendation

First re-check this class AFTER Group 04's `update_root_snapshot` deploy-
throughput fix lands — it may resolve automatically. If it persists
independently, read `testDeploy`/`testServlets`' source for the specific
`assertTrue` condition and add targeted logging to capture actual vs.
expected (same bare-assertion limitation as
[[form-authenticator-cookie-session-bare-assertion]]).
