# TestGroupChannelSenderConnections — intermittent SIGSEGV in NioSender.keepalive

**Status:** OPEN. Pre-existing on `dev`; found while closing
[22-tribes-realnetwork-membership-bug](../../internal/fixed-suite-bugs/tomcat/22-tribes-realnetwork-membership-bug-FIXED.md)
and deliberately **not** fixed there — different subsystem (TCP replication
sender), different root cause.

## Symptom

`org.apache.catalina.tribes.group.TestGroupChannelSenderConnections` crashes the
VM part-way through `testConnectionLinger`, roughly 1 run in 6:

```
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF736980850
#  Faulting access: read at address 0x000002B624B75B84
#  thread: "main-vm"
#  gc collector: generational   young-gen policy: non-moving (STW mark-sweep young)
#  jit: faulting pc not attributed to a compiled method
#  Java frames:
#    ... TestGroupChannelSenderConnections.testConnectionLinger
#    ... GroupChannel.send -> ChannelCoordinator.sendMessage
#    ... ReplicationTransmitter.sendMessage -> PooledParallelSender.sendMessage
#    at org/apache/catalina/tribes/transport/PooledSender.returnSender
#    at org/apache/catalina/tribes/transport/nio/ParallelNioSender.keepalive
#    at org/apache/catalina/tribes/transport/nio/NioSender.read
Native frames: exe+0x1263562, external/jit, external/jit, external/jit, exe+0xF60850
```

Three of the five native frames are `external/jit`, so the fault is reached
from JIT-compiled code. The class also fails non-fatally in two other ways at a
similar rate — an assertion failure, and
`GroupChannel.messageReceived Unable to deserialize message:[ClusterData…]` —
which may or may not share the root cause.

Purely TCP: `ParallelNioSender` / `NioSender`. No datagram code is involved, so
this is unrelated to the multicast membership work in doc 22.

## Evidence that it predates the doc-22 fixes

Interleaved A/B on the same host, alternating binaries run-by-run so host load
is shared, 12 runs each:

| binary | PASS | CRASH | other FAIL |
|---|---|---|---|
| clean `origin/dev` @ `fa1b0ade0` | 9 | **2** | 1 |
| `origin/dev` + doc-22 fixes | 8 | **2** | 2 |

Identical crash rate. (An earlier non-interleaved sample read 3/16 vs 6/16, but
that comparison was confounded by concurrent host load — the interleaved run is
the one to trust.)

The class PASSes on HotSpot in the same fixture, and has PASSed on CratonVM in
several prior full-suite runs, so it is intermittent rather than always-broken.

## Reproduction

```powershell
$env:CRATONVM_REAL_NET_SOCKETS='1'; $env:CRATONVM_REAL_AQS='1'
<cratonvm.exe> -Xmx2g -Djava.net.preferIPv4Stack=true -cp <tomcat suite cp> org.junit.runner.JUnitCore org.apache.catalina.tribes.group.TestGroupChannelSenderConnections
```

Run it ~10 times; expect 1–2 access violations. Re-run with
`CRATONVM_DBG_JIT_NAMES=1` to attribute the faulting pc to a compiled method,
and consider `CRATONVM_DISABLE_JIT=1` to confirm the JIT dependency before
digging further.
