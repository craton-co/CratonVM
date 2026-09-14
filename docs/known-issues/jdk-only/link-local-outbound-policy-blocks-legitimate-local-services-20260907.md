# The link-local/cloud-metadata outbound block is broader than SSRF protection needs — two independent test failures so far

| | |
|---|---|
| **Status** | OPEN (by-design behavior with an overly broad blast radius, not a crash/correctness bug). |
| **Mechanism** | `native-io/src/{net.rs,datagram.rs,lib.rs}` — CratonVM refuses outbound TCP/UDP connections to link-local addresses (IPv4 `169.254.0.0/16`, IPv6 `fe80::/10`) by default, an SSRF-style protection aimed at cloud metadata endpoints (`169.254.169.254` and friends). |

## Symptom

```
java.io.IOException: connect denied by outbound policy: resolved address <fe80::...>
  of [<fe80::...>%N]:<port> is blocked: link-local cloud-metadata address <fe80::...>
  is blocked by default policy
```

## Confirmed instances

1. **`org.h2.test.server.TestAutoServer`** (2026-09-07 full H2 suite run) —
   H2's auto-server discovery connects over a link-local IPv6 address on the
   loopback-adjacent interface; the connection is refused, `FAIL`.
2. **`org.apache.catalina.tribes.test.channel.TestRemoteProcessException`**
   (Netty/Tomcat session, documented in
   `docs/known-issues/tomcat/tribes-multicast-family-still-environmental.md`)
   — Tribes cluster membership tries to reach a peer's link-local IPv6
   address; same refusal. That page correctly notes this is "not the reason
   this test suite fails" (the whole Tribes family is independently
   environmental — multicast doesn't work on that host/network at all,
   confirmed against HotSpot) but flags the policy itself as "worth a closer
   look."

## Why this is not simply "fix it"

The policy is intentional and its target — literal cloud-metadata IPs
(`169.254.169.254`, the IPv6 equivalents) — is a real SSRF vector worth
blocking by default. The current implementation appears to block the **whole
link-local range** rather than the specific well-known metadata addresses (or
metadata-serving ports), which also catches legitimate same-link
communication: local auto-discovery services, cluster membership protocols,
and anything else that happens to bind or route over a link-local address on
a real network interface.

## Disposition

Neither the H2 nor the Tribes instance is being chased as a "fix" — both are
recorded so a future session doesn't re-diagnose "connect denied" as a new
correctness bug. If link-local networking support becomes a priority (cluster
protocols, service discovery, anything IPv6-link-local), the actionable next
step is narrowing the block to the specific known metadata addresses/ports
rather than the entire `fe80::/10` / `169.254.0.0/16` ranges — not attempted
in this session.

## Reproducing

```bash
cd apps/h2database-suite-runner
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-h2-suite.sh run \
  --only 'TestAutoServer' --tag repro   # FAIL: connect denied by outbound policy
```

## Related

- `docs/known-issues/tomcat/tribes-multicast-family-still-environmental.md`
  — the original discovery of this mechanism, in a different suite.
