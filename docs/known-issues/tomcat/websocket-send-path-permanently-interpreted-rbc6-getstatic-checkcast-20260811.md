# `TestAsyncMessagesPerformance` — the WebSocket send path is permanently interpreted, and RBC.6 names why

| | |
|---|---|
| **Status** | OPEN — cause named, fix not attempted |
| **HotSpot** | `OK (1 test)` |
| **CratonVM** | `testAsyncTiming` fails on timing only — **0 framing failures** |
| **Discovered** | 2026-08-11, running the check `29-throughput-wall-recurrence-and-unconfirmed` asked for |

## This is the answer to a question an older page left open

`29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md` recorded this class as
"Frame sizes are correct; only the inter-chunk timing counters trip. Pure
per-write latency, not a framing bug", and then said of the whole family:

> `CRATONVM_DBG_JITC=1 CRATONVM_DBG_RBC6=1` names the gate in one run. It is
> the obvious next step for …

That run had not been done for this class. It has now, and it names the gate.

## The failure is timing, confirmed by counting

The test prints which sub-condition tripped; that output is easy to lose to a
grep, and losing it is the difference between "known perf wall" and "framing
bug". Counted over one run:

| | failures | budget |
|---|---|---|
| `Expected size …` (framing) | **0** | 0 |
| `SEQ0: Expected diff > 40ms` | 16 | 1 |
| `SEQ1: Expected diff < 500,000` | 16 | 10 |
| `SEQ2: Expected diff < 500,000` | 362 | 100 |

Every chunk arrives at the right size; all three latency counters blow their
budget. The older page's read is exactly right.

`SEQ0` is worth noting: it expects a gap **greater** than 40 ms (the server's
deliberate 50 ms pause) and saw 20.5 ms. Messages are not merely late, they
bunch — a chunk delayed into the pause window shortens the next observed gap.

## Named cause

`CRATONVM_DBG_JITC=1 CRATONVM_DBG_RBC6=1` over the run. **Every** compile-bail
in the whole process is the same gate — `rbc6-handler-reads-unsafe-local` — and
three of the four distinct sites are the per-message send path itself:

```
WsRemoteEndpointImplBase.startMessage(BLjava/nio/ByteBuffer;ZLjakarta/websocket/SendHandler;)V   pc=169, op=0xb2  (getstatic)
WsRemoteEndpointImplBase.endMessage(Ljakarta/websocket/SendHandler;Ljakarta/websocket/SendResult;)V  pc=35,  op=0xc0  (checkcast)
NioEndpoint$NioSocketWrapper$NioOperationState.run()V                                            pc=23,  op=0xb2  (getstatic)
IntrospectionUtils.setProperty(…)Z                                                               pc=84,  op=0xbe  (arraylength)
```

`startMessage` frames and queues every message, `endMessage` completes every
message, and `NioOperationState.run` is the NIO write runner. All three are
refused compilation outright and stay interpreted for the whole run — on a test
that sends 4500 messages and measures the gaps between them.

Two further per-message sites are inlining refusals rather than bails:
`WsRemoteEndpointImplServer$OnResultRunnable.<init>` and
`SendResult.<init>`, both `reason=precise-exception-frames`; plus
`OutputBufferSendHandler.<init>` at `reason=callee-too-large`.

## Why this looks closable, and what the risk is

The same shape has already been closed once. `rbc6-protected-field-ops-FIXED-20260802`
found that RBC.6's exclusion of `getfield`/`putfield` "outlived its cause by
five days" — the precise-frame capability the exclusion demanded had already
been built in two earlier commits and nobody revisited the admission list. The
fix was one predicate plus an A/B opt-out.

`precise_frame_publishing_opcode` (`jit/src/lib.rs`) currently admits:

* `0xb7` `invokespecial`, `0xb8` `invokestatic`, `0xc2` `monitorenter`,
  `0xc3` `monitorexit` — unconditionally
* `0xb6` / `0xb9` virtual+interface invokes — behind `precise_virtual_invokes_enabled`
* `0xb4` / `0xb5` `getfield`/`putfield` — behind `precise_field_ops_enabled`

The three opcodes blocking this path — `0xb2` `getstatic`, `0xc0` `checkcast`,
`0xbe` `arraylength` — are not on it.

**The fix is not "add three opcodes".** Each has to actually publish a precise
frame at its trapping bci before it may be admitted, and their throw paths are
not the same as a field op's:

* `getstatic` has no receiver and cannot NPE; it throws out of *class
  initialisation* and resolution, so the frame has to exist on the `<clinit>`
  and resolution-failure edges, not on a null check.
* `checkcast` throws `ClassCastException` from the type-test failure edge.
* `arraylength` throws NPE on a null array, which is the closest to the
  already-admitted field shape.

Admitting an opcode whose frame is not really built hands a handler a frame
that was never written — a miscompile that only shows up when an exception
actually crosses that site, which is exactly the class of bug the 08-02 page
warns about. Anyone taking this on should follow that page's method: verify the
publishing site per opcode first, then extend the predicate, and keep a
`CRATONVM_JIT_NO_PRECISE_*` opt-out so one binary can be A/B'd against itself.

## Reproduction

```bash
source /data/toolchain/env.sh
cd apps/tomcat
CP=$(cat .suite/cp-linux-fixed.txt)
CRATONVM_DBG_JITC=1 CRATONVM_DBG_RBC6=1 <cratonvm> --java-home /data/toolchain/jdk-25 \
  -cp "$CP" org.junit.runner.JUnitCore \
  org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance
```

Do not filter stdout to JUnit's own lines — the `SEQ0:`/`SEQ1:`/`SEQ2:`
diagnostics the test prints are the only thing separating a framing bug from a
latency one, and `Assert.assertFalse(handler.hasFailed())` reports neither.
