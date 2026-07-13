# TestAbstractAjpProcessor.testSecret — AJP `secret` attribute not enforced (403 expected, 302 actual)

**Status:** OPEN. **Severity:** low (single test). **HotSpot:** PASS
(fresh-verified). Uncovered after fixing the whole-class connection failure
— see
`docs/internal/tomcat-08-07/abstractajpprocessor-socket-not-connected-FIXED.md`.

## Summary

`org.apache.coyote.ajp.TestAbstractAjpProcessor.testSecret` configures the
connector with `secret=RIGHTSECRET`, then sends an AJP forward-request with
NO secret attribute at all and expects the connector to reject it with a
403 response:

```
java.lang.AssertionError: expected:<403> but was:<302>
	at org.apache.coyote.ajp.TestAbstractAjpProcessor.validateResponseHeaders(TestAbstractAjpProcessor.java:961)
	at org.apache.coyote.ajp.TestAbstractAjpProcessor.testSecret(TestAbstractAjpProcessor.java:533)
```

CratonVM instead returns a 302 (looks like normal request processing
proceeded — likely a context/welcome-file redirect — rather than the AJP
processor rejecting the request for a missing/wrong `secret` attribute).
The second half of the same test (sending an explicit wrong secret,
`WRONGSECRET`, attribute `0x0C`) was not reached/verified in isolation —
worth checking whether it also fails once this is investigated.

Found via a fresh Windows rerun (dev tip after
`fix/dohead-family-regressions-v2-20260713` merged, 2026-07-13, real JDK,
JIT on) immediately after fixing the connection-establishment bug that
previously made all 30 tests in this class fail uniformly. HotSpot passes
all 30 tests including this one.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName ajpsecret `
  -Start <idx> -Count 1 -TimeoutSec 120 -Parallel 1
# org.apache.coyote.ajp.TestAbstractAjpProcessor (testSecret)
```

## Recommendation

Since `org.apache.coyote.ajp.AjpProcessor` is real Tomcat bytecode (no
CratonVM native override found for it or for AJP message/attribute
parsing), the bug is more likely in one of: (a) how
`Connector.setProperty("secret", ...)` (a generic reflective/
`IntrospectionUtils`-based property setter) actually threads the value down
to the AJP processor/protocol handler — if the property silently fails to
apply, the processor would never enforce any secret check and just process
requests normally (consistent with a redirect instead of a 403); or (b) how
CratonVM's AJP wire-protocol byte reading handles a forward-request that
has NO secret attribute (as opposed to a wrong one) — the request might be
mis-parsed such that it looks like a *different*, valid request shape.
Start by confirming with a targeted probe whether
`AbstractAjpProtocol.setSecret`/`.getSecret()` (or wherever the "secret"
JavaBean property lands) actually holds `"RIGHTSECRET"` right before the
first request is processed.
