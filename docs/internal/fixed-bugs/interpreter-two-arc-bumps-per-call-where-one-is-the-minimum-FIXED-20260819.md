# The static call path took two `Arc` refcount bumps where one is the minimum — FIXED 2026-08-19

**Status:** FIXED. `frame_build` −18%; ~3.4% off a zero-argument interpreted
call (conservative reading, see below).

**Instruments:** `CRATONVM_DBG_INVOKE_PHASES=1`, `probes/ZeroArgCallProbe.java`.

## The two targets were one root cause

The phase decomposition left two open targets, and they looked independent:

| phase | cyc/call | share |
|---|---|---|
| frame_build | 128.5 | 33.8% |
| ic_lookup | 92.0 | 24.2% |

Two measurements collapsed them into one:

* `size_of::<CachedInvokeTarget<RetainedCode>>()` is **56 bytes** — about one
  cache line, ~8 cycles to copy. So the entry clone charged to `ic_lookup` is
  not expensive because of its size. The **`Arc` refcount bump inside it** is
  the cost.
* `size_of::<Frame>()` is 296 bytes, but `frame_build`'s other resident is a
  *second* bump of the **same** `Arc`.

Counting the static hot path:

1. `t.clone()` at the inline-cache probe — bumps the `cached` Arc (charged to
   `ic_lookup`)
2. `cached.clone()` at the frame build — bumps it **again** (charged to
   `frame_build`)

Two atomic increments per call and two matching decrements on pop, where **one
of each is the minimum**: the cache keeps its own reference and the frame needs
its own. The second was pure waste — `target` is an owned local that dies at the
end of the arm, so the frame can take the Arc by **move**.

Reading either function on its own shows one innocuous `.clone()`. Only the
measurement makes the pair visible, which is the argument for having
instrumented rather than acted on the structural suspicion directly.

## The fix

Bind the `Bytecode` arm by value instead of `ref`, and hand the `Arc` to
`Frame::new_pooled_cached` by move. Two call sites then needed an explicit `&`,
and **the compiler found both** — type errors rather than silent behaviour
change, which is the failure mode this path deserves.

Steady-state refcounts are unchanged: cache holds one, frame holds one. Only the
transient bump/drop pair disappears.

### Scope checked, not assumed

The **virtual** hot path already moves (`dispatch_virtual` frame builds at 2172
and 2754 take `cached` by value); only the static path had the double clone, so
this is not applied by symmetry. `dispatch_virtual:934`'s `Arc::clone` genuinely
cannot move — `entry_cached` is used again at 958 — but that is the
inline-cache **fill** path after a miss, not the hot path.

Every guard between the lookup and the frame build was enumerated first
(redefine/native-shadow, continuation, two debug gates,
`cached_static_owner_stale`, the entry gate), because skipping one would be a
correctness bug rather than a slow path. None were moved or removed.

## Measurements

**Phases** (6,400,348 zero-argument `invokestatic` calls, same probe, same host):

| phase | OLD cyc | NEW cyc | OLD share | NEW share |
|---|---|---|---|---|
| ic_lookup | 57.8 | 53.6 | 26.4% | **27.7%** |
| guards | 26.9 | 25.0 | 12.3% | **12.9%** |
| args | 27.1 | 24.5 | 12.4% | **12.6%** |
| **frame_build** | **79.0** | **64.8** | **36.1%** | **33.4%** |
| frame_push | 28.2 | 25.9 | 12.9% | **13.3%** |

`frame_build` −18%, and its **share fell while every other share rose** — the
signature of a targeted win in that phase rather than a run that was uniformly
faster.

**Wall clock**, interleaved, order reversed at round 5: **NEW faster in 8 of 8
rounds** (1.7, 5.6, 5.0, 1.8, 4.6, 7.1, 6.8, 4.6%).

**The quoted figure is the conservative one.** The 8-round design conflates
position with time — position-1-OLD occurs only in rounds 1-4 — so a
slot-versus-slot reading is not clean here. Rounds 1-4 are the safe subset:
**NEW ran second, the disadvantaged slot, and was still faster in all four**,
median ~3.4%. Rounds 5-8 give ~5.7% with OLD in the penalised slot, so ~3.4% is
quoted as the floor rather than the midpoint.

## A prediction that was wrong, recorded

Before measuring, this record predicted wall clock would fall by MORE than the
phase table showed, on the grounds that the removed decrement fires when
`target` drops — after the last phase probe, so uncharged.

It did not. The phase total fell ~25 cyc/call while wall clock fell ~18. The two
are not directly comparable in the first place (phases are measured with the
instrument on, wall clock with it off), so the reasoning was too clever for the
evidence available. The honest reading is that both instruments agree in
direction and rough magnitude, and that is all they were ever able to say.

## What this leaves

`ic_lookup` remains the number-two phase at ~27% and is now down to the
**minimum one** `Arc` bump; what is left there is the `FxHashMap` hash and
bucket probe plus the 56-byte entry copy. A direct-mapped table would remove the
hash and probe but **not** the copy or the bump, so it is worth less than the
raw share suggests — the reason to measure before building one.

`frame_build` remains the largest phase. What is left in it is the 296-byte
`Frame` itself: built, then moved by value into the frame stack, ~5 cache lines
written per call and read again on pop, where HotSpot's interpreter frame is a
few words on the native stack. Shrinking `Frame` — boxing the cold fields out of
the hot path — is the next structural move, and it is a much larger change than
this one.

And, unchanged: roughly **half** the fixed per-call cost is outside these phases
entirely, in the callee body and the return / frame-pop path, which nothing here
measures.
