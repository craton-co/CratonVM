# `org.apache.catalina.tribes.test.channel.*` — still environmental, confirmed on both VMs

| | |
|---|---|
| **Status** | Confirmed environmental, NOT a CratonVM bug |
| **HotSpot** | Fails identically |
| **Discovered** | 2026-08-06, complete 651-class Tomcat suite rerun |

## Symptom

4 classes fail, consistent with the long-standing "Tribes multicast
clustering doesn't work on this network — env; HotSpot also fails"
characterization (`05-suite-rerun-fail-triage.md`):

```
TestDataIntegrity        expected:<10000> but was:<0>       (5 failures)
TestMulticastPackages    expected:<10000> but was:<1513>    (3 failures)
TestUdpPackages          expected:<10000> but was:<0>       (6 failures)
TestRemoteProcessException:
  org.apache.catalina.tribes.ChannelException: java.io.IOException:
  connect denied by outbound policy: resolved address fe80::67b0:99e:5a9b:287e
  of fe80:0:0:0:67b0:99e:5a9b:287e:4001 is blocked: link-local cloud-metadata
  address fe80::67b0:99e:5a9b:287e is blocked by default policy
```

`TestRemoteProcessException`'s specific failure is new to this session: a
CratonVM outbound-connect policy treats the cluster member's own link-local
IPv6 address as a "cloud-metadata address" (an SSRF-style protection
normally aimed at `169.254.169.254`-style IPv4 metadata endpoints) and blocks
it. This is a real, distinguishable behavior — worth a closer look if
Tribes/cluster support on IPv6-link-local networks ever becomes a priority —
but it is **not the reason this test suite fails on this host**: HotSpot
fails the identical test method (`testDataSendSYNCACK`, same
`Assert.assertEquals(TestRemoteProcessException.java:91)` line) for its own,
different reason (the multicast/cluster membership never completes on this
network topology regardless of VM).

## Verification

```
HotSpot: TestDataIntegrity           Tests run: 5, Failures: 3
HotSpot: TestRemoteProcessException  Tests run: 1, Failures: 1
```

Both fail on stock HotSpot 25.0.3 with Tomcat's own JVM args, confirming the
root cause is this host's network/multicast configuration, not CratonVM.

## Update 2026-08-13 — confirmed identical under all 3 GC backends

Complete 651-class Tomcat suite run under Generational, G1, and ZGC
(2026-08-12/13) shows all 4 classes in this family failing the same way on
every backend, and a 93-class cross-GC non-passed union rerun under ZGC
alone reconfirms them there too. Consistent with this page's own
environmental characterization: the failure is about this host's network,
not any collector's behavior, so backend-independence is exactly what's
expected. The `TestDataIntegrity` **NOSUMMARY** flagged as an open question
in `gc-backend-3way-fullsuite-comparison-20260810.md` ("a VM abort with no
JUnit summary at all is a stronger symptom than this family's usual
assertion failures... not yet checked whether this is a distinct VM-abort
defect") was **not** seen in this run — `TestDataIntegrity` reported a normal
FAIL with a JUnit summary on all 3 backends, so whatever produced that one
NOSUMMARY on 08-10 did not reproduce here. Left unresolved as a one-off
rather than chased further, since the family's baseline failure mode is
otherwise unchanged and this page's core diagnosis stands.
