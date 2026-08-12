# `TestAsyncMessagesPerformance` — the WebSocket send path is permanently interpreted, and RBC.6 names why

| | |
|---|---|
| **Status** | OPEN — two of the three original opcodes admitted 2026-08-11; **the test did not move**. 2026-08-12: the doc's own recommended cheap experiment run — one method fully unblocked, moved nothing; the other has at least 4 stacked unadmitted opcodes, not 1. **Recommend not pursuing further** — see bottom |
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


---

## Update 2026-08-11 — `getstatic` and `checkcast` admitted, and it changed nothing measurable

`precise_frame_publishing_opcode` now admits `0xb2` and `0xc0`
(`fix/rbc6-getstatic-checkcast-arraylength-20260811`). Neither needed new
codegen — both were already publishing on every path that can throw, and the
admission list had simply never been revisited, exactly as
`rbc6-protected-field-ops-FIXED-20260802` found for `getfield`/`putfield`. The
per-opcode argument is on `precise_getstatic_checkcast_enabled`.

**What it bought: one method.** `WsRemoteEndpointImplBase.endMessage` (the
`0xc0` site) now compiles. That is all.

**What it did not buy.** The other two did not become compilable — the bail
moved to the next unadmitted opcode in the same protected range:

| method | before | after |
|---|---|---|
| `WsRemoteEndpointImplBase.startMessage` | `pc=169, op=0xb2` getstatic | `pc=172, op=0x13` **ldc_w** |
| `NioEndpoint$…$NioOperationState.run` | `pc=23, op=0xb2` getstatic | `pc=44, op=0x12` **ldc** |
| `IntrospectionUtils.setProperty` | `pc=84, op=0xbe` arraylength | unchanged |

**And the test did not move.** Timing failures before → after: SEQ0 16 → 35,
SEQ1 16 → 22, SEQ2 362 → 344, against budgets of 1/10/100. Still 0 framing
failures. The host was loaded for both runs, so read this as "unchanged", not
as a regression — but it is certainly not an improvement.

### The part worth arguing about before spending more

One of the three hot methods *did* become compilable and the numbers did not
respond. That is weak evidence that JIT admission is not the binding constraint
on this test at all. Before doing the `ldc`/`ldc_w` work, someone should
establish that compiling these methods is worth anything — e.g. by measuring
the send path with all three compiled (a hand-built binary that admits `ldc`
unsafely would answer it in one run, without shipping anything), or by
profiling where the inter-chunk gap actually goes. This family has a long
history of named-lever-moves-nothing: see the retired WebFlux, AQS and
native-funnel pages.

### If the `ldc` work does get done

`ldc`/`ldc_w` are genuinely unpublished, and in two different ways:

* the String path calls `helpers.ldc_string` (it allocates) with **no**
  `emit_post_invoke_exception_check` after it;
* the Class path resolves through `helpers.ldc_class_cp`, which can run a user
  `ClassLoader.loadClass` and throw, and whose `0`-return guard branches to the
  **shared** sentinel stub — no frame at the bci.

So this is not another bookkeeping correction; it is new codegen on two arms,
and the Class arm's `0`-return convention differs from the `i64::MIN` sentinel
that `emit_post_invoke_exception_check` tests for.

`arraylength` (`0xbe`) remains unadmitted for the reason given above, and is now
the foil in `protected_instanceof_is_precise_exception_covered`.

### Verification of the admission itself

* `cratonvm-jit`: 14 suites green, including two new tests —
  `rbc6_admits_exactly_the_opcodes_whose_lowerings_publish` and
  `rbc6_gate_clears_getstatic_and_checkcast_but_not_arraylength`.
* Tomcat regression slice on the new binary: `TestTomcat` (26),
  `TestStandardContext` (27), `TestDirResourceSet` (40), `TestJMXAccessorTask`
  (1) — 94 tests, all green.
* `vm --test jit_local_exception_handler_tests` fails 3
  (`test_jit_exception_in_handler_not_recaught_by_same_handler`,
  `test_jit_indy_after_side_effect_no_double_execution`,
  `test_precise_handler_frame_catches_a_throw_at_the_end_of_its_try`).
  **Pre-existing**, established by re-running the same binary with
  `CRATONVM_JIT_NO_PRECISE_GETSTATIC_CHECKCAST=1`: identical three failures.
  The opt-out was itself proved live first — with it set,
  `rbc6_gate_clears_getstatic_and_checkcast_but_not_arraylength` fails on the
  checkcast assertion — so that A/B is not a vacuous green.

---

## 2026-08-12 — the doc's own recommended experiment, run

The "part worth arguing about before spending more" section above asked for
exactly this: measure the send path with the remaining hot methods compiled,
via a hand-built binary that admits the blocking opcodes unsafely, before
doing the real `ldc`/`ldc_w` codegen work. Done, on current `dev`
(worktree `CratonVM-wsrbc6-20260812`, binary rebuilt fresh — see
[reference_diagnostic_binary_caveats](#) — not shipped, not committed).

**Method:** `precise_frame_publishing_opcode` (`jit/src/lib.rs`) edited
locally, four times in sequence, each admitting one more opcode
unconditionally with **no** frame-publishing fix behind it — a deliberate
miscompile risk accepted only because this binary runs one local JUnit class
and is discarded, never shipped: no commit ever carried this change, and the
worktree it was made in was reverted to a clean diff against `dev` before
this write-up. Each step rebuilt and reran `TestAsyncMessagesPerformance`
with `CRATONVM_DBG_JITC=1 CRATONVM_DBG_RBC6=1`.

| step | opcode admitted | `startMessage` | `NioOperationState.run` | SEQ0 | SEQ1 | SEQ2 |
|---|---|---|---|---:|---:|---:|
| baseline | (none) | bail `pc=167,0x13` ldc_w | bail `pc=44,0x12` ldc | 0 | 9 | 476 |
| hack 1 | `0x12`/`0x13` ldc/ldc_w | **compiles** | bail `pc=51,0xba` invokedynamic | 0 | 8 | 484 |
| hack 2 | + `0xba` invokedynamic | compiles | bail `pc=141,0x32` laload | 0 | 8 | 484 |
| hack 3 | + `0x32` laload | compiles | bail `pc=404,0xbf` **athrow** | 0 | 39 | 480 |

(framing failures: 0 throughout, all four runs — matches every prior
measurement on this page.)

### Finding 1: `startMessage` compiling changed nothing measurable

The only method that actually went from interpreted to compiled across all
three hacks is `startMessage` (message framing/queueing) — `endMessage` was
already fixed on 2026-08-11, `NioOperationState.run` never compiled in any
variant (see Finding 2). SEQ0 stayed 0 and SEQ1/SEQ2 stayed in baseline's
rough range in hacks 1 and 2 (8/484 vs baseline's 9/476). This is the same
"named-lever-moves-nothing" shape the page already flagged for `endMessage`
— now confirmed for the second of the three original methods too.

### Finding 2: `NioOperationState.run`'s protected range has (at least) 4 stacked unadmitted opcodes, not 1

The doc's earlier read — "blocked by `ldc`" — undercounted. Clearing each
opcode only exposed the next one already sitting in the same protected
range: `ldc` (0x12) → `invokedynamic` (0xba) → `laload` (0x32) →
**`athrow` (0xbf)**. The method never actually compiled in any of the three
hacks, so its own effect on the timing numbers is still **unmeasured**.

`athrow` is not a bookkeeping gap like `getstatic`/`checkcast` were — this
same file's own comment on `may_throw_without_precise_frame` already lists
it alongside `new` and `arraylength` as "still genuinely blocking, because
their lowerings really do not publish". It is the throw instruction itself;
admitting it is a materially different (and by the page's own risk model, a
materially more dangerous) undertaking than the `ldc`/`ldc_w` work this page
originally scoped, and was never on the table as a "next opcode" to just
add. There may be more opcodes past it — the search stopped here rather
than continuing to chase a method whose easiest blocker is already the hard
kind.

### A caution about this test's noise floor

Hacks 1, 2 and 3 all have the **identical actual compiled-method set** —
`NioOperationState.run` bailed in all three, so nothing about which code
ran actually changed between them. Yet SEQ1 reads 8, 8, then 39. That jump
has no code explanation; it is host contention (this run shared the box
with 6 concurrently-running full Tomcat-suite shards). Treat any single run
of this test's SEQ counters as noisy by a factor of several×, matching the
"host was loaded for both runs" caution the 2026-08-11 update already gave
— this just puts a number on how noisy.

### Recommendation

**Do not pursue the `ldc`/`ldc_w`/`invokedynamic`/`laload`/`athrow` codegen
work for this test.** The evidence now points the same direction the
2026-08-11 update already suspected, from a second independent angle:
compiling the one method that *could* be cheaply tested (`startMessage`)
moved nothing, and the method most likely to actually matter
(`NioOperationState.run`, the NIO write runner) is blocked behind a
materially harder opcode than originally scoped, with unknown depth beyond
it. Whoever revisits this should either profile where the inter-chunk gap
actually goes (the 08-11 update's other suggested next step, still not
done) or accept this as a known, understood perf wall and move on — not
resume the opcode-admission whack-a-mole where this session left off.
