# A statically-bound callee with an exception table pays the dispatch funnel — 10.2x, and an OSR body cannot escape it at all

**Status: the static-door ban is LIFTED (default-ON, 2026-08-21). The OSR half
is OPEN and is now the whole of this page's remaining content.**

> **The flip was made against this page's own recommendation, deliberately.**
> The section below still says the census does not justify it, and that has not
> changed — no workload win was found or is claimed. It was flipped on the
> consistency argument instead: the statically bound door, which is the EASY
> case, was 11x worse than the virtual door that had already been lifted on the
> same evidence. A default that makes the simpler path the slower path is not a
> conservative default. The correctness case for lifting it is
> `probes/ExcTableDirectCallOracle.java` (below); the performance case is that
> there isn't one, and the page says so rather than being rewritten to agree
> with the decision.
Found 2026-08-21 while pricing `ir_unresumable_protected_trap`, where a hot
method with a `try`/`finally` measured ~12.8x the same arithmetic without one on
BOTH tiers. That gap is not the tier and not that rule. It is this.

## The finding

A static callee whose only difference is a **never-taken** `try`/`catch` goes
through `jit_invoke_dispatch` on every call. Directly observed, not inferred —
`CRATONVM_DBG_JIT_DISPATCH=1`, 600 000 iterations:

```
1187000  JIT_DISPATCH] NativeFunnelFloorProbe.guardedStatic
      0  (no line for NativeFunnelFloorProbe.plain)
```

`jit::direct_call_exc_table_publish_enabled` is the gate, it is **default-OFF**,
and its own doc says *"Default-OFF pending its own measurement … not worth
defaulting on unmeasured."* Here is the measurement.

`probes/NativeFunnelFloorProbe.java`, ABBA on ONE binary, 3 rounds, 6 runs per
arm, ns/op:

| rung | ban kept (default) | ban lifted | |
|---|---:|---:|---|
| **static call, callee has `try`/`catch`, via an ordinary frame** | **107.30** | **10.57** | **10.2x** |
| static call, same callee, from an **OSR** loop body | 102.68 | 100.26 | unmoved |
| *control* plain static call, no table | 7.1 | 7.1 | flat |
| *control* interface call, callee has `try`/`catch` | 9.2 | 9.2 | flat |

`sha256(0..7)` is byte-identical in both arms and against HotSpot 25.

## Two separate facts, and the second is the one nobody has recorded

**1. The ban costs 10.2x, and its sibling door was already lifted on less.**
The virtual/interface door had the identical ban for the identical stated
reason. It was measured at **8.7x** on this same probe and lifted;
`CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=0` now restores it. The control row above is
that fix working — the same callee shape through an interface costs 9.2 ns while
the static path costs 107. **The static door is currently 11x worse than the
virtual door for the same callee**, which is the wrong way round: a statically
bound site is the easy case.

**2. An OSR body never consults the gate at all.** The second row does not move
when the switch is flipped, and the engagement counter says why —
`CRATONVM_DBG=intrinsic-stats` reports

```
direct callee binds: 0 bound, 0 left on the dispatch helper
  bind refused, unattributed: 0
```

in **both** arms. Zero sites examined, zero refusals tallied: an OSR body emits
its own invokes and never reaches `callee_compiler`, so the gate is not merely
off there, it is unreachable. **A hot loop calling an exception-table callee
therefore pays the funnel forever, switch or no switch** — and a hot loop is
exactly where this costs.

The `via hop` rung exists to separate those two, and it is why the number is
trustworthy: the same callee, one extra ordinary frame, 107.30 -> 10.57. Without
that rung the flat OSR row would read as "the gate does not work".

## What this is worth

The shape is `try { … } finally { … }` in a small method called from a hot path
— resource release, buffer cleanup, lock release. netty's
`ObjectUtil.checkPositive(x, "increment")` inside `RefCnt.retain0`, every
`ByteBuf` release path, every `AutoCloseable` helper.

Note what this does NOT claim. The 10.2x is per-CALL on a callee of this shape;
it is not a workload number. The previous page in this chain
(`ir-unresumable-trap-refusal-cost-ANSWERED-20260821.md`) is a worked example of
why those differ — a 0.43 % population turned a real per-method refusal into
nothing measurable. The census below asks exactly that question of this
refusal, using `direct callee binds: N bound, M left on the dispatch helper`
plus `bind refused, callee-exception-table: K` — and gets the same answer.

## The census, and why the flip is NOT justified on this evidence

Done 2026-08-21, same binary, real netty classes, `CRATONVM_DBG=intrinsic-stats`:

| class | ms | bound | left on helper | of which `callee-exception-table` |
|---|---:|---:|---:|---:|
| `handler.ssl.SslHandlerTest` | 75 670 | 1 587 | 1 252 | **228** |
| `handler.codec.http2.Http2MultiplexCodecTest` | 46 388 | 1 920 | 877 | 73 |
| `handler.codec.compression.ZstdDecoderTest` | 12 996 | 592 | 356 | 38 |
| `util.concurrent.NonStickyEventExecutorGroupTest` | 34 532 | 219 | 187 | 22 |
| `buffer.SimpleLeakAwareByteBufTest` | 81 715 | 2 221 | 931 | 18 |
| `buffer.BigEndianHeapByteBufTest` | 20 267 | 1 523 | 732 | 17 |
| `handler.codec.http.HttpContentCompressorTest` | 6 966 | 96 | 117 | 3 |

`native-shadow` dominates every one of these (671 of 732 on the last).

Then ABBA on the two best candidates, one binary:

| class | ban kept | ban lifted | within-arm spread |
|---|---:|---:|---|
| `BigEndianHeapByteBufTest` | 23 609 / 22 293 | 21 274 / 23 728 | ~10 % |
| `SslHandlerTest` (228 refusals) | 39 646 / 31 643 | 44 101 / 36 174 | ~39 % |

**No measurable workload effect**, on the class chosen precisely because it
refuses 13x more than the others. Correctness holds in both arms —
`ok=414 failed=0` and `ok=53 failed=0 aborted=1`, identical.

So the switch's "pending its own measurement" is **answered, in the negative**:
the per-call multiplier is 10.2x and real, the population is 3-228 sites per
class, and the two do not multiply into anything a workload can see. Flipping
the default is a consistency argument — the static door being 11x worse than the
virtual door for the same callee shape is genuinely ugly — not a measured win.

**It was flipped on 2026-08-21 on exactly that basis**, with the performance
verdict left standing above. What changed was not the evidence but the weight
put on the asymmetry with the already-lifted virtual door.

### Why the two findings are coupled

The gate applies only to callees reached through `callee_compiler` — i.e. from
an ORDINARY compiled frame. A hot loop is an OSR body, and an OSR body never
consults the gate at all (finding 2). So the sites that could benefit most from
a direct call are **systematically excluded from the population the switch can
act on**: what is left is, by construction, the colder half.

That is a real possibility and this page does not claim it as fact — but it
means "lift the ban and measure" cannot answer the question while the OSR gap
stands. **The OSR gap should be closed first, and the flip re-measured after**,
rather than the flip being retried on more workloads.

## Next steps, in order

1. ~~**Census**~~ — done, above.
2. ~~**Flip the default**~~ — DONE 2026-08-21, see "The flip" below.
3. **Give OSR bodies the direct-bind ladder**, or record why they cannot have
   it. This is the larger of the two and it is independent of the switch.

## The flip (2026-08-21)

`CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH` is default-ON; `=0` restores the ban.
The interlock is untouched and is **not** a knob: `SP_IC_DEOPT_CHECK != On`
still forces the ban back on whatever the flag says, because publishing a direct
`CALL` while the `i64::MIN` sentinel check is suppressed is the unsound state the
MIC sibling's page already recorded being fooled by (207.04 ns/op with the
interlock holding, 24.12 without it — a refusal read as a pass).

**The correctness case, which is the only thing that mattered.** The ban had one
stated reason: *"a raw `CALL` has no Rust frame to notice the `i64::MIN`
sentinel and run the callee's own handler."*
`probes/ExcTableDirectCallOracle.java` falsifies it directly — a callee's own
handler, its `finally`, a wrong-type handler that must NOT catch, a nested
catch, and a rethrow-of-a-different-type, including the **implicit** AIOOBE /
NPE / div-by-zero cases which are the ones that actually leave through the
sentinel. Output is byte-identical across HotSpot 25, the new default, and the
ban restored.

The probe routes every guarded call through an ordinary frame (`driver`), not
from its own loop — an OSR body never consults the gate, so a probe that called
these methods from its hot loop would pass without ever baking the direct `CALL`
it claims to test. Engagement, `CRATONVM_DBG=intrinsic-stats`:

| arm | binds | `callee-exception-table` refusals |
|---|---|---|
| default (lifted) | 2 bound, 26 left | **absent** |
| `=0` (ban) | 0 bound, 23 left | **16** |

**Validation.** `cargo test -p cratonvm-jit` (15 binaries), `-p cratonvm-jit-api`
and `-p cratonvm-types` green; the `cratonvm-vm` suite green; netty
`BigEndianHeapByteBufTest` 414/414, `ZstdDecoderTest` 8/8,
`Http2MultiplexCodecTest` 63/63, `NonStickyEventExecutorGroupTest` 10/10 in the
new default, with the first two also run under the ban for comparison.
`jit::direct_exc_table_publish_policy` pins the default and the interlock as a
pure decision table — no environment mutation, because `set_var` in a parallel
test binary is a data race rather than a visibility question.

## Reproduce

```bash
cratonvm --java-home <jdk> -cp <out> NativeFunnelFloorProbe 4000000 40
CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH=1 cratonvm ... (same)
```

Prove which rung takes the funnel, rather than inferring it from the timing:

```bash
CRATONVM_DBG_JIT_DISPATCH=1 cratonvm ... NativeFunnelFloorProbe 600000 6 2>&1 \
  | grep -oE 'JIT_DISPATCH\] NativeFunnelFloorProbe\.[a-zA-Z_]+' | sort | uniq -c
```

And that the gate is not even consulted from the OSR rung:

```bash
CRATONVM_DBG=intrinsic-stats cratonvm ... 2>&1 | grep -E 'direct callee binds|bind refused'
```

## Related

* `ir-unresumable-trap-refusal-cost-ANSWERED-20260821.md` — where the 12.8x was
  first seen, recorded there as an untested hypothesis (*"inlining is the
  obvious hypothesis … not confirmed"*). It was not inlining: both callees are
  compiled and neither is inlined, because an OSR body passes an empty
  `inline_sites` map. It is the funnel.
* `jit::direct_call_exc_table_publish_enabled` — the gate, its interlocks, and
  the 16-of-892 population note that is the only prior number attached to it.
* `vm/src/jit/helpers.rs::mic_publish_exception_table_callees` — the sibling
  door, measured at 8.7x and already lifted.
