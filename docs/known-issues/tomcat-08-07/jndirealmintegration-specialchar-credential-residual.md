# TestJNDIRealmIntegration — special-character credential authentication residual

**Status:** OPEN. **Severity:** medium. **HotSpot:** not yet checked.
**Related:**
[jndirealmintegration-ldap-connection-npe-FIXED.md](../../internal/tomcat-08-07/jndirealmintegration-ldap-connection-npe-FIXED.md)
(`docs/internal/tomcat-08-07/`) — that doc's connection-level
`SocketFactory`/`socketLock` NPE bug is confirmed genuinely fixed (a fresh
rerun no longer shows any `LDAPException`/`NullPointerException` at
connect time), but the class now has a distinct, real residual.

## Summary

`org.apache.catalina.realm.TestJNDIRealmIntegration` now runs all 76 tests
(previously 0 ran at all due to the connection-level bug) but 15 fail with
a plain assertion, all in `testAuthentication` with two credential
patterns:
```
1) testAuthentication[11: user[<>+="#;,rrr], pwd[<>+="#;,rrr]](org.apache.catalina.realm.TestJNDIRealmIntegration)
java.lang.AssertionError
	at org.junit.Assert.assertNotNull(Assert.java:713)
```
- **11 failures** share the exact special-character credential
  `<>+="#;,rrr` (parameters 11/17/23/29/35/41/47/53/59/65/71 — every 6th
  index, suggesting this credential is repeated across multiple realm/
  connection-pool configurations the test matrix covers).
- **4 failures** share a plain credential `user[testsub], pwd[test]`
  (parameters 72/73/74/75 — the last 4```` indices in the 76-test run).

Found via a fresh Windows full-suite rerun (dev commit `f23a3f42a`,
2026-07-14, real JDK, JIT on, `CRATONVM_REAL_NET_SOCKETS=1` etc. set).

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName jndildap2 `
  -Start <idx> -Count 1 -TimeoutSec 120 -Parallel 1
# org.apache.catalina.realm.TestJNDIRealmIntegration
```

## Recommendation

Verify against fresh HotSpot first (not yet done for this specific
residual) to rule out a fixture/environment gap before assuming a genuine
CratonVM bug — this suite has an established pattern this session of
fixture-staging issues on this host. If HotSpot passes cleanly, the two
failure groups point in different directions worth investigating
separately:
- The `<>+="#;,rrr` special-character group: likely an LDAP DN/filter
  escaping difference — check whether CratonVM's real-mode LDAP client
  path (or a string-escaping helper it goes through) handles these
  RFC 4514/4515-special characters (`<`, `>`, `+`, `=`, `"`, `#`, `;`, `,`)
  differently than HotSpot when building the bind DN or search filter.
- The `testsub`/`test` group (last 4 parameters, likely a distinct realm
  configuration variant near the end of the parameter matrix — check
  `TestJNDIRealmIntegration`'s `@Parameters` source for what's special
  about indices 72-75): read the test source to identify what
  configuration these last 4 cases exercise before assuming it's related
  to the special-character group.
