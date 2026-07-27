# Tribes real-network membership/coordination bug — 2 classes

**Status:** OPEN. Confirmed CratonVM-only regression — both classes PASS on
HotSpot in the same fixture. Reproduced consistently across two independent
runs (`dev` ~60a710ad8 and ~12c79a0ee).

## Symptom

- `org.apache.catalina.tribes.group.interceptors.TestTcpFailureDetector` —
  `testTcpFailureMemberAdd`:
  ```
  java.lang.AssertionError: Expecting member count to not be equal expected:<1> but was:<0>
  ```
  A member that should have been added to the group (count should become
  nonzero, i.e. not equal to the initial baseline) never registers — member
  count stays at 0.

- `org.apache.catalina.tribes.group.interceptors.TestNonBlockingCoordinator` —
  `testCoord1`:
  ```
  java.lang.AssertionError: Member count expected to be equal. expected:<9> but was:<4>
  ```
  Fewer than half the expected members (4 of 9) are visible to the
  coordinator.

## Analysis

Both classes exercise Apache Tribes' real UDP/TCP group-membership protocol
(multicast heartbeats / TCP failure-detection pings between simulated
cluster members) under `CRATONVM_REAL_NET_SOCKETS=1`. Both symptoms are
"expected member count higher than actual" — consistent with membership
announcements/heartbeats being dropped, delayed past a liveness timeout, or
never sent/received correctly over CratonVM's real-socket networking stack.
Plausibly one shared root cause (a gap in UDP multicast or TCP keep-alive
handling under real sockets), not investigated down to the exact native
networking code path in this session.

## Reproduction

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <ref-marking-these-2-non-PASS> -TimeoutSec 300 -Parallel 2 -RunName tribes-repro -Exe <cratonvm.exe>
```
Compare against `-Vm hotspot` — both pass on HotSpot with the same real
sockets, same fixture, same timeout.
