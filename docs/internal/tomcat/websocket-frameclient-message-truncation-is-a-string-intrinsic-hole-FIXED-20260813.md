# `TestWebSocketFrameClient(SSL)#testConnectToServerEndpoint` — not truncation. The `java/lang/String` JIT intrinsics were inert in two of the three compile doors

| | |
|---|---|
| **Status** | **FIXED 2026-08-13**, `fix/websocket-frame-truncation-20260813`. Both classes green: `OK (4 tests)` / `OK (6 tests)`. |
| **Supersedes** | `websocket-frameclient-message-truncation-20260813.md` (same day), whose framing — "large WebSocket message truncated mid-read" — was wrong. Nothing is truncated. |
| **Also closes** | the last open residual of `websocket-send-path-permanently-interpreted-rbc6-getstatic-checkcast-20260811.md` — see the bottom section. |

## The original doc's read, and why it was wrong

The 2026-08-13 record described an `AssertionError: expected:<100000> but was:<N>`
with `N` varying run to run, called it a truncated message, and reasoned:

> Non-deterministic, partial-but-nonzero, and consistently well under half the
> expected 100000 bytes — the shape of a connection or read loop terminating
> early under a race …

Three things in that sentence are not what the test measures.

1. **100000 is not bytes.** `TesterFirehoseServer.MESSAGE_COUNT` is a MESSAGE
   count, and each message is `MESSAGE_SIZE = 1024` bytes. The assertion is
   `assertEquals(MESSAGE_COUNT, handler.getMessageCount())` — 100000 *messages*,
   ~102 MB of payload.
2. **Nothing terminates early.** The client's `wsSession.isOpen()` is still
   `true` at the assertion, the server-side error count is 0, and the server's
   send loop is still running when the clock runs out.
3. **The number is a stopwatch reading, not a byte offset.** The test waits
   `TesterFirehoseServer.WAIT_TIME_MILLIS = 300000` for its latch and then
   asserts whatever arrived. `N` is simply *how far a rate-limited stream got in
   300 seconds*, which is exactly why it is different every run and different
   per GC backend: it tracks host load, not the collector.

Given enough time the run completes with **every** message. Measured on this
branch with the pre-fix binary on an idle host: `recv=100000 sent=100000
wall=148.0s`, `latch=true`, 0 errors. The three GC backends were never the
variable.

## The instrument

`FirehoseProbe2` — an instrumented clone of the test with a second
`@ServerEndpoint` that reports its own send progress — prints both sides once a
second, which is what separates "the stream stopped" from "the stream is slow":

```
[probe] t=48.1s recv=32330 (+1378, 688/s) sent=32335 (+1380, 689/s) open=true err=0
```

`recv` tracks `sent` to within ~10 messages for the whole run. There is no
divergence between what the server wrote and what the client read at any point,
which rules out the entire family the original doc proposed (multi-frame
reassembly, a fixed buffer size, a read loop dropping a tail). The two sides are
in lockstep at ~680 msg/s, against HotSpot's ~200,000 msg/s on the same box
(0.5 s for the whole 102 MB).

## Where the 300x went

`--stack-sample-ms 5` over the run, split by thread. The server's
`http-nio-…-exec-N` thread carries ~93 % of all samples — it is CPU-bound, not
blocked — and its leaves are almost entirely one method:

| deepest named frame | share |
|---|---:|
| `Utf8Encoder.encodeNotHasArray` pc=47 (`last_pc=44`) | 52.0 % |
| `Utf8Encoder.encodeNotHasArray` pc=86 (`last_pc=83`) | 26.0 % |
| everything else | 22.0 % |

`javap` maps both: pc=44 is `invokevirtual java/nio/CharBuffer.get:()C` and
pc=83 is `invokevirtual java/nio/ByteBuffer.put:(B)Ljava/nio/ByteBuffer;`. The
samples land at the instruction *after* each invoke with the "active JIT call"
note attached, i.e. the leaf is the callee. So 78 % of the send path is two
single-element buffer accessors, called 1024 + 1024 times per 1 KB message.

`Utf8Encoder.encodeLoop` takes that per-character path because
`WsRemoteEndpointImplBase` encodes `CharBuffer.wrap(text)`, and
`CharBuffer.wrap(CharSequence)` returns a read-only `StringCharBuffer` whose
`hasArray()` is `false` by specification. HotSpot runs the identical Java. The
difference is entirely per-call cost.

### Per-call cost, measured

`BufAccessorProbe` / `CharAtProbe`, 2-20 M iterations inside an OSR-compiled
loop, CratonVM vs HotSpot on the same host:

| operation | CratonVM (before) | HotSpot |
|---|---:|---:|
| `char[]` element load | 1.0 ns | 0.3 ns |
| `String.length()` | 28.2 ns | ~0 ns |
| **`String.charAt(i)`** | **408 ns** | **0.6 ns** |
| `StringCharBuffer.get()` | 524 ns | 0.5 ns |
| `HeapByteBuffer.put(byte)` | 173 ns | 0.8 ns |

A raw array store is already at HotSpot's level, so the compiled loop body is
fine. `String.charAt` at 408 ns is not a slow method — it is a method whose
**intrinsic never fired**, so every call ran the real chain
`charAt → isLatin1 → StringLatin1.charAt → String.checkIndex →
Preconditions.checkIndex`, the last of which is a registered native.

## Root cause: there are THREE compile doors, and the String intrinsics were in one

`jit/src/lib.rs` has a complete, unit-tested `java/lang/String` call-site
intrinsic family — `length`/`isEmpty`/`charAt`/`hashCode`/`equals`/`compareTo`/
`indexOf` — with a resolver (`try_resolve_string_intrinsic`), an x86-64 codegen
ladder (`x64/bytecode_walk.rs`, `INTRINSIC REGION … STRING_ACCESS`), narrow-oop
and compact/legacy layout handling, deopt stubs, and two green integration
suites. All of it was dead on every hot path.

Every call-site intrinsic in this VM is registered in exactly ONE place: the
single-pass invoke loop near the end of `try_compile_inner`. There are three
doors into compiled code, and only one of them reaches that loop:

1. **`try_compile_inner`, single-pass (C1)** — reaches the loop. The intrinsics
   work here, which is why the unit tests are green.
2. **`try_compile_inner`, optimizing (C2 / IR)** — when `ir_lower` produces a
   body the function `return`s *before* the loop. A method that tiers up
   silently LOSES every intrinsic it had at C1.
3. **`jit_bridge`'s OSR door** — reaches `x64::compile_with_param_slots`
   directly with its own copy of the direct-call ladder, and passed
   `string_layout: None` under the comment *"String intrinsics land in a later
   wave"*. The wave never came. A hot loop is compiled by this door.

That three-door shape is not a new discovery — `native-call-funnel-per-call-floor-item2-20260805`
names it in as many words ("binding it in all THREE compile doors is the whole
lesson of that document") after the `Thread.currentThread()` bind reported 0
bypasses across 8,000,000 calls while installed in doors 1 and 2. This is the
same species, one layer up: not a thin direct-call helper but the whole
call-site intrinsic pass.

**No timing could have found it.** A resolver, a codegen ladder and two green
suites all agree the intrinsic exists; "installed but never offered to this
body" and "installed and no faster" produce the identical table.

## The fix

Two changes, both in the doors that were missing it.

**Door 3 (OSR).** `jit_bridge` now resolves the String field layout once per
OSR compile, offers every `invokevirtual`/`invokeinterface` site to
`try_resolve_string_intrinsic`, and hands the SAME layout to
`compile_with_param_slots` — matcher and codegen must agree or the codegen falls
through and `CALL`s an intrinsic sentinel address. The same door also gained the
layout-independent STATIC families (`Math`/`StrictMath`, `Integer`/`Long` bit
ops) via `try_resolve_intrinsic`; it previously recognised exactly one of them,
`Math.sqrt`, by hand. Restricted to `invokestatic` so `guard_class_id: 0` is
right by construction and the CRC32 members — the only ones needing a resolved
receiver guard — cannot reach this path.

**Door 2 (optimizing tier).** The IR lowerer binds direct calls to real function
addresses only; it has no representation for an intrinsic sentinel, so it cannot
simply be taught the same list. Instead a method carrying a String access site
is **pinned to the single-pass backend**, exactly as PERF-01 already pins a
method whose loop the single-pass backend vectorises: *where the intrinsic
fires, an IR body is a downgrade*. Deliberately narrow — the String family only,
and only when a layout actually resolved. Opt out with
`CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1`.

A third change is a permanent lever, not a fix: `CRATONVM_DBG_INTRINSIC=1`
prints one line per single-pass invoke site and one per IR body, so "did this
site even get offered an intrinsic, and which door compiled it" is a one-run
question instead of two build-and-measure cycles.

### What it bought

Per-call, same probes as above:

| operation | before | after |
|---|---:|---:|
| `String.charAt(i)` | 408 ns | **1.6 ns** |
| `String.length()` | 28.2 ns | **1.1 ns** |
| `StringCharBuffer.get()` | 504 ns | **146 ns** |
| `HeapByteBuffer.put(byte)` | 173 ns | 160 ns (untouched — see below) |

The `StringCharBuffer.get()` row is an **in-binary A/B**, not a cross-run
comparison: the same binary with `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` reads
491.7 ns and without it 146.0 ns, with `String.charAt` at 1.4 ns in both arms
(that row is door 3's fix, which the flag does not gate).

End to end, `FirehoseProbe2` at the test's real 100000 messages, both arms run
back to back on a host that happened to be carrying another build — i.e. under
the same kind of load the suite run had:

| binary | result |
|---|---|
| pre-fix | `recv=68051 sent=68057 wall=300.0s latch=false` — **the doc's failure, reproduced** |
| fixed | `recv=100000 sent=100000 wall=254.0s latch=true` — **passes** |

And the two real classes, on the fixed binary:

```
[run-one] org.apache.tomcat.websocket.TestWebSocketFrameClient    rc=0 wall=293.9s   OK (4 tests)
[run-one] org.apache.tomcat.websocket.TestWebSocketFrameClientSSL rc=0 wall=262.0s   OK (6 tests)
```

## What is still slow, and what was measured and rejected

Re-profiling after the fix, the send thread's leaves are now `ByteBuffer.put(B)`
35.4 % and `CharBuffer.get()` 32.9 %. `ByteBuffer.put(B)` is a *forced native*
(`native_override`), and its ~150 ns is the JIT's native **dispatch** floor, not
its body: a decomposition against a plain user virtual call (17 ns) puts
`bb.capacity()` — one field read — at 89 ns.

**A per-`ClassId` field-name → slot memo for `get_field_by_name`/
`set_field_by_name` was written, measured, and reverted.** The hypothesis was
that `bb_storage_view` + `buf_set_position` perform five to seven hierarchy
walks per `put(byte)` at ~13 ns each. On an interleaved A/B the memo moved the
NIO accessors by 1-3 %, inside the noise band of a plain user virtual call in the
same table. The walk is not the cost; the dispatch floor is. Recorded here so it
is not re-derived: the lever for that floor is item 3 of
`native-call-funnel-per-call-floor-item2-20260805` (generalising the per-callsite
native dispatch cache beyond its three hand-written families), which that doc
deliberately leaves open, and which changes native-versus-bytecode precedence
for every compiled call site in the VM.

## Not the already-known permanent gaps in this area

The original doc's section on this stands unchanged and is repeated because it
is still true: `TestSsl#testClientInitiatedRenegotiation[JSSE]` and
`TestClientCert#testClientCertPostZero[JSSE]` fail because rustls does not
implement TLS 1.2 renegotiation (a deliberate CVE-2009-3555/3SHAKE mitigation).
Both are carved out as permanent residuals by
`testssl-client-initiated-renegotiation-FIXED` and
`21-tls-handshake-enforcement-gap-FIXED`, at exactly "1 failure" each. Unrelated
to this doc, before or after.

## Verification

* `cratonvm-jit --lib`: 1989 passed, 0 failed (includes the new
  `a_string_access_site_pins_its_method_to_the_single_pass_backend`, whose foil
  arm asserts a method with no String site is NOT demoted — a pin that fired on
  everything would quietly put the whole program on C1).
* `cratonvm-vm --lib`: 2495 passed, 0 failed.
* Tomcat regression slice on the fixed binary, all green:
  `TestTomcat` (26), `TestStandardContext` (27), `TestDirResourceSet` (40),
  `TestByteChunk` (8), `TestCharChunk` (3), `TestMessageBytes` (8),
  `TestStringCache` (1), `TestB2CConverter` (7), `TestUDecoder` (60),
  `TestWsFrame` (2), `TestWsRemoteEndpoint` (8), `TestWsSession` (9).
  The `buf` classes are deliberate: they are the densest `String`/`CharChunk`
  users in the tree and so the population the pin moves off the optimizing tier.

## Reproduction

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
apps\tomcat-suite-runner\run-one.ps1 -Class org.apache.tomcat.websocket.TestWebSocketFrameClient -Exe <cratonvm.exe>
apps\tomcat-suite-runner\run-one.ps1 -Class org.apache.tomcat.websocket.TestWebSocketFrameClientSSL -Exe <cratonvm.exe>
```

The per-call rungs, which answer in seconds rather than minutes:

```bash
<cratonvm> -cp <dir> CharAtProbe      # String.charAt / length / char[] baseline
<cratonvm> -cp <dir> BufAccessorProbe # StringCharBuffer.get / ByteBuffer.put
CRATONVM_DBG_INTRINSIC=1 <cratonvm> … # which door compiled each body
```

---

## Closing the residual of the RBC.6 send-path doc

`websocket-send-path-permanently-interpreted-rbc6-getstatic-checkcast-20260811`
ended with two suggested next steps and a recommendation not to resume the
opcode-admission work. Its remaining open item was:

> Whoever revisits this should either **profile where the inter-chunk gap
> actually goes** (the 08-11 update's other suggested next step, still not done)
> or accept this as a known, understood perf wall and move on.

Done, with `--stack-sample-ms 5` over `TestAsyncMessagesPerformance` on this
branch. **The send path is not CPU-bound at all.** Over a 37 s run the sending
`http-nio-…-exec-4` thread produced 574 samples against the sampler's ~7400 —
about 7.7 % of wall — and **86.1 % of even those land at `pc=78, last_pc=75` in
`TesterAsyncTiming$Endpoint.onMessage`, which is the instruction after the
test's own `Thread.sleep(50)`.** No thread in the process is CPU-saturated.

So the inter-chunk gap is not produced by the cost of the send path's code, and
compiling more of it cannot close it — which is the same conclusion that page
reached twice (once from `endMessage` compiling and moving nothing, once from
`startMessage` compiling and moving nothing), now from a third and independent
angle. That page's residual is **closed**: the gap is latency in the async
write → completion → next-send handoff, and the opcode work
(`ldc`/`ldc_w`/`invokedynamic`/`laload`/`athrow`) must not be resumed.

The fix in this document does not move that test and was not expected to:
`TesterAsyncTiming` sends `ByteBuffer`s through `sendBinary`, so it never enters
`Utf8Encoder` at all. Measured A/B anyway, because "expected not to move" is a
prediction and not a result — pre-fix `SEQ0=0 SEQ1=1 SEQ2=279`, post-fix
`SEQ0=0 SEQ1=0 SEQ2=299`, framing failures 0 in both, i.e. unchanged inside that
test's own several-fold noise band.
