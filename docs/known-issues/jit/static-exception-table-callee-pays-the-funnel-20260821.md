# A statically-bound callee with an exception table pays the dispatch funnel — 10.2x, and an OSR body cannot escape it at all

**Status: OPEN, with the measurement its own switch has been waiting for.**
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
nothing measurable. **Someone should census how many such call sites a real
workload executes before sizing this.** The census instrument already exists:
`direct callee binds: N bound, M left on the dispatch helper` plus
`bind refused, callee-exception-table: K`.

## Next steps, in order

1. **Census** `callee-exception-table` refusals and their execution counts on a
   real workload (tomcat, netty suite, h2). The per-call number above is the
   multiplier; the census is the population.
2. **Flip `direct_call_exc_table_publish_enabled` to default-ON** if the census
   supports it. It is one line, and the interlocks it needs are already written
   and enforced: `sp_ic_deopt_check_mode() == On`, and the emitter must have
   reserved the contiguous service-argument slots that
   `emit_inline_callee_deopt_check` needs (a site that cannot is a compile
   failure, `direct-call-service-slots`, not an unserviced raw edge). **This
   needs the vm suite run, not just the probe** — it changes codegen for every
   statically bound exception-table callee.
3. **Give OSR bodies the direct-bind ladder**, or record why they cannot have
   it. This is the larger of the two and it is independent of the switch.

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
