# What the unresumable-trap refusal costs: 0.43 % of methods, and < 8 ns/iter on the ones it declines — ANSWERED 2026-08-21

**Status: ANSWERED.** The residual said the refusal had no price attached and
that scoping the deopt-resume capability without one would be guesswork. It has
a price now, and the price is **not a reason to build that capability**.

`jit::ir_unresumable_protected_trap` declines the optimizing tier to any method
whose protected range carries BOTH a deopt-guarded opcode and a side effect. The
rule is correct and untouched by this work.

| question | answer |
|---|---|
| how many methods does it decline? | **62 of 14 421** admitted (0.43 %), across 30 netty classes |
| how many classes see any refusal? | **11 of 30** |
| what does it cost a hot declined method? | **nothing measurable — < ~8 ns/iter**, see the bound below |
| what kind of code does it decline? | channel/stream **lifecycle**, not inner loops |

## The instruments

Both landed with this work, default-off in effect and documented at the switch:

* **A census** — `CRATONVM_DBG_JIT_METHOD_STATS=1` prints
  `IR unresumable-trap refusal: shape=N refused=N`, unconditionally and
  including zeros. Two counters because the A/B needs both halves: with the
  guard off, `shape` keeps counting and `refused` drops to zero, so their
  difference names how many methods the OFF arm actually moved. One counter
  cannot tell *"the shape is rare"* from *"the switch is not wired"*.
* **`CRATONVM_JIT_IR_UNRESUMABLE_TRAP_GUARD=0`** — MEASUREMENT ONLY and unsound
  in general. It does not make the deopt resumable; it stops the compiler
  avoiding it. The refusal keys on a STATIC shape while the unsoundness needs
  the trap to FIRE, so on a workload that does not throw there the OFF arm is a
  correct program and a fair price.

## Population: 0.43 % of admitted methods, in lifecycle code

30 netty JUnit classes, counts (not timings, so host contention is irrelevant):

| class | admitted | refused |
|---|---:|---:|
| `handler.codec.http2.Http2MultiplexCodecTest` | 2 058 | 26 |
| `handler.codec.http2.HttpToHttp2ConnectionHandlerTest` | 1 478 | 14 |
| `buffer.BigEndianDirectByteBufTest` | 2 768 | 8 |
| `handler.codec.compression.ZstdDecoderTest` | 622 | 4 |
| `util.concurrent.NonStickyEventExecutorGroupTest` | 166 | 3 |
| `buffer.SimpleLeakAwareByteBufTest` | 2 764 | 2 |
| 5 more classes | — | 1 each |
| 19 classes | — | 0 |
| **total** | **14 421** | **62** |

Three earlier workloads for scale: `CratonBench` 2 admitted / 0 refused,
`NettyZipBombPhases snappy` 297 / 0, `ByteBufUtilTest` 458 / 0.

**What it declines matters as much as how many.** All 11 refusals on
`HttpRequestDecoderTest`, named:

```
EmbeddedChannel.writeInbound / finish / close
EmbeddedChannel$EmbeddedUnsafe$1.register / close / beginRead
AbstractChannel$AbstractUnsafe.beginRead
AbstractChannel$AbstractUnsafe$6.run
ChannelOutboundBuffer.close
HttpObjectDecoder.handlerRemoved0
DefaultHeaders.fromObject
```

Channel and handler **lifecycle** — which is where Java puts `try`/`finally`,
because that is where resources are released. It is not where the inner loops
are. The shape the residual worried about (`try { buf[i] = x; flush(); }
finally { … }`) is real and it is teardown code.

## Cost: not measurable, with the resolution stated

**A whole-class wall-clock cannot answer this and should not be quoted for it.**
`HttpRequestDecoderTest` ABBA, 12 runs: GUARD-ON 7 228 ms mean against
GUARD-OFF 7 585 ms — but the host drifted from load 7.18 to 11.68 across the run
and the within-arm spread was 6 368–8 100 ms (27 %). A 0.43 %-of-methods effect
is not resolvable inside that. Recorded here only so nobody re-runs it expecting
an answer.

`probes/UnresumableTrapShapeRate.java` prices ONE method of the declined shape
instead, hot, on one binary. **The probe is verified to be declined for the
RIGHT reason** — the admission ladder is ordered and several terms sit ahead of
this rule:

```
guard ON : shapedOnce: an inline trap this tier deopts on (pc=4, opcode=0x2e) …
           flatOnce  : admitted to the optimizing pipeline
           shape=1 refused=1
guard OFF: shapedOnce: admitted to the optimizing pipeline
           shape=1 refused=0
```

ABBA, 3 rounds, 6 runs per arm, Windows host, 20 M iterations each:

| arm | `shaped` (declined) ns/iter | *control* `flat` ns/iter |
|---|---:|---:|
| GUARD-ON (declined, single-pass) | 172.1 173.8 172.2 178.0 170.6 176.1 → **173.8** | 13.5 |
| GUARD-OFF (admitted, optimizing) | 171.1 177.6 178.5 170.5 174.9 172.7 → **174.2** | 13.7 |

**0.24 % apart, inside a within-arm spread of ~8 ns (4.6 %), with the control
flat.** Giving the tier back to a hot method of this shape buys nothing.

The honest form of that claim is a BOUND, not a zero: the probe could not have
detected an effect smaller than its ~8 ns/iter spread, so what is established is
**< ~8 ns/iter on a method of this shape**, against a body that costs 174.
Combined with 0.43 % of methods, and those being lifecycle code, there is no
route from this rule to a workload number worth having.

## The thing this measurement DID surface, which is not this rule

Look at the control column. The same arithmetic **without** an exception table
costs **13.5 ns/iter**; wrapped in `try`/`finally` it costs **174** — about
**12.8x** — and that gap is **tier-independent**, identical in both arms.

So the expensive thing about these methods is not which backend compiles them.
It is having an exception table at all.

**ROOT-CAUSED 2026-08-21 — and it was NOT inlining.** See
`osr-door-refused-every-exception-table-callee-FIXED-20260824.md`. Both callees are
compiled and NEITHER is inlined (an OSR body passes an empty `inline_sites`
map), so the inlining hypothesis recorded here was wrong. What actually happens
is that a statically-bound callee declaring an exception table is barred from a
baked direct `CALL` and takes `jit_invoke_dispatch` instead — observed directly,
1 187 000 funnel entries for the guarded callee against zero for the unguarded
one. Lifting that ban (`CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH=1`) moves the rung
107.30 -> 10.57 ns/op, 10.2x, controls flat.

Recorded here as an untested hypothesis and now replaced by a measured
mechanism, which is the point of having written it that way.

That gap is worth roughly 160 ns/iter on every hot `try`-carrying method. This
rule is worth < 8. Anyone scoping work in this area should take the first
number, and this page exists to stop them taking the second.

## Reproduce

```bash
cratonvm --java-home <jdk> -cp <out> UnresumableTrapShapeRate 20000000 20
CRATONVM_JIT_IR_UNRESUMABLE_TRAP_GUARD=0 cratonvm ... (same)
```

Verify the probe before believing either number:

```bash
CRATONVM_DBG_IR_COMPILES=1 CRATONVM_DBG_JIT_METHOD_STATS=1 cratonvm ... 2>&1 \
  | grep -E 'admission.*(shapedOnce|flatOnce)|unresumable-trap refusal'
```

Population census over a class list:

```bash
CRATONVM_DBG_JIT_METHOD_STATS=1 CRATONVM_DBG_IR_COMPILES=1 \
  cratonvm --java-home <jdk> -cp <suite-cp> CratonRunner <class>
```

## Related

* `ir-exception-stub-throw-bci-test-cannot-reach-its-own-tier-FIXED-20260821.md`
  — where this residual was recorded, and the test whose probe this same rule
  declined.
* `unresumable-unconditional-trap-mvmap-FIXED-20260802.md` — why the rule
  exists, and the *"do not apply the publish-side rule blind"* warning that gave
  it the two narrowing terms which keep the population at 0.43 %.
