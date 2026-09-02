# The optimizing tier's reference store, and the gate that was a constant

*2026-09-02. Branch `perf/jit-ir-ref-store-20260902`. Follows
`gen-gc-jit-ref-store-gates-20260902.md`, which published the barrier plan and
named this tier as the place the plan was inert.*

## What this was supposed to be, and what it turned out to be

The intended change was small and was named in the previous page's residual
list: `ir_lower.rs` lowered every `Op::Store(MemKind::Ref)` to
`jit_putfield_object` unconditionally, so the reference-store barrier plan the
generational collector publishes could not reach the tier where hot loops are
compiled. Give that tier the same gated arm the single-pass backend already
has, and the plan starts paying.

The arm was the easy part. What it exposed is the page:

1. **The plan's own SATB pre-gate was pinned ARMED for the life of the
   process.** Every compiled reference store that consulted it — on either
   tier, under either publishing collector — bailed to the full helper. This is
   why the plan measured no throughput change when it landed.
2. **A compact-only store shape fires almost never**, because the TLAB
   allocation fast path writes legacy headers unconditionally.

Neither was visible in the census that shipped with the plan, and the reason
they were not is the most portable thing here.

## The census that could not see it

`gen-gc-jit-ref-store-gates-20260902.md` reported engagement as
`gated=2 declined=0` and read that as the fast path being live. It is a
**compile-time** count: two sites got the gate sequence emitted. Whether either
one ever *takes* its fast path is a different fact, and the two were not
separated anywhere.

So this change added the separation —
`CRATONVM_DBG_IR_REF_STORE_TRACE=1`, a `LOCK INC` on each path plus one per
bail reason, off by default and never on in a timed arm. The first reading, on
`bench/RefStoreLoopProbe` under `-XX:+UseGenerationalGC`:

```
[cratonvm] optimizing-tier reference stores: gated=2 declined=0
[cratonvm]   ir ref-store executions: inline=0 helper=16384000
[cratonvm]     ir ref-store bail satb-marking-armed: 16384000
```

Two sites gated, sixteen million executions, **zero** of them inline. ZGC read
identically. `gated=2` was an instrument armed where it could not fire.

## Defect 1 — a gate nothing ever lowered

`publish_jit_ref_store_plan{,_masked}` stored a hard `1` into `pre_active`, with
the argument that a plan should start armed and be relaxed by a later publisher
call:

```rust
// before
JIT_REF_STORE_GATES.pre_active.store(1, Ordering::Release);
```

There is no later publisher call. `pre_active` is recomputed only by
`refresh_jit_ref_store_pre_active`, which runs when a marker arms or disarms
(`ConcurrentGcState::set_phase`) or when ZGC flips its own mirror
(`set_mark_active`). On a run whose collector never enters a concurrent mark
phase, neither happens — so the byte keeps the value publication gave it, for
the life of the process, and every gated store pays the call it was gated to
avoid.

The fix is to publish the disjunction the gate is defined as:

```rust
// after
refresh_jit_ref_store_pre_active();
```

Safe for the reason the hard `1` was reaching for. At publication the heap
being constructed is not yet reachable, so it has no marking of its own; any
*other* heap's marking is already counted in `JIT_REF_STORE_PRE_MARKERS`, which
is why that counter is a count and not a boolean. The result can only be
conservatively armed, never falsely permissive.

**The regression test gets its own binary** (`gc/tests/ref_store_pre_gate.rs`).
The gate and the marker count behind it are process-global, and a census inside
the `gc` unit-test binary — ~1800 tests, dozens of heaps in mark phases —
read **29 markers armed**. The state the test is about, "nothing is marking",
does not occur there, so the assertion would have been vacuous. It was checked
against the old behaviour and fails on it.

## Defect 2 — the shape that almost never applies

With the pre-gate fixed the census moved one gate along and stayed at zero:

```
[cratonvm]   ir ref-store executions: inline=0 helper=16384000
[cratonvm]     ir ref-store bail receiver-not-compact: 16384000
```

`init_object_header` — the TLAB fast path that serves nearly every allocation
for the interpreter and `jit_new_object` alike — writes a LEGACY header
unconditionally: `array_length = 0`, no `GC_FLAG_COMPACT`, whatever compact
layout the class has registered, because it never consults `plan_object_alloc`.
Its own comment says so. So an arm that stores inline only into genuinely
compact receivers stores inline almost never.

This is the same trap the IR tier's inline `getfield` fell into and climbed out
of on 2026-08-18, where the compact-only read turned out to be 100% of
`jit_getfield`'s calls on Generational. The fix is that fix: emit **both**
shapes and pick per OBJECT on the header bit, exactly as that arm and
`jit_putfield_object` itself do. The legacy shape — tag qword then pointer
payload in the uniform 16-byte `Value` cell — is transcribed from the
single-pass backend's own legacy inline reference store rather than re-derived,
so the two cannot disagree about which half of the cell holds what.

## What the arm emits

Verified by decoding the compiled body, not by reading the emitter
(`CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=RefStoreLoopProbe.churn`):

```
mov  r11, <pre gate>      ; cmp byte [r11], 0 ; jne -> helper
movzx ecx, byte [rax+0Fh]                     ; the flags byte, read ONCE
mov  r11d, [rax+4]        ; mov r10, <idx> ; cmp r10d, r11d ; jae -> drop
test cl, 1                ; GC_FLAG_OLD_GEN — young receiver, no card
jz   -> store
mov  r11, <post gate>     ; cmp byte [r11], 0 ; jz -> store   ; no old objects
jmp  -> helper                                ; a barrier is genuinely needed
store:
mov  rdx, <value>
test cl, 4                ; GC_FLAG_COMPACT — which shape does THIS object have
jz   -> legacy
mov  [rax+10h], rdx       ; compact: the bare 8-byte pointer
jmp  -> done
legacy:
mov  r10, 4 ; mov [rax+10h], r10 ; mov [rax+18h], rdx   ; the 16-byte cell
```

One header byte answers both questions asked of it: `GC_FLAGS_BYTE_OFFSET`
packs `gc_age` in bits 4..7 and the GC flags in bits 0..3, so bit 0 is the
post-barrier question and bit 2 is the layout question, off one `movzx`.

Nothing is proven twice. The receiver's containment check is the same function
the inline `getfield` read uses — one transcription of six compares against a
six-word table, not two. `JIT_REGION_BOUNDS` is still never consulted here, so
the G1/ZGC interlock that leaves it empty is untouched; those collectors
publish no plan and decline at the fourth gate, which the census shows as
`no-published-barrier-plan: 2`.

## The measurement

`bench/RefStoreLoopProbe` — a flat counted loop whose body is almost nothing
but reference stores into young receivers, allocation-free in the steady state,
and which reads every store back and throws if one was lost. Same binary, three
arms, **order alternated by round**:

| arm | median wall |
|---|---|
| gated arm on | **0.98 s** |
| `CRATONVM_JIT_IR_REF_STORE=0` | 3.16 s |
| `CRATONVM_GC_JIT_REF_STORE_GATES=0` | 3.11 s |

6 of 6 rounds sign-consistent, **~3.2x**, all three arms printing identical
output. (An earlier run of the same three arms on a busier host read 1.27 /
3.62 / 3.65 s — 8 of 8, ~2.85x. The ratio moves with host load; the ordering
does not.) The third arm is the control the second cannot be on its own: it
shows the single-pass gates contribute nothing on this workload, so the whole
effect belongs to this arm.

**Order alternation is not a formality here.** The first pass of this A/B ran a
fixed arm order and reported a clean 15% win with 4 of 4 rounds
sign-consistent. Alternating the order made it vanish — the host drifted
upward within each round (2608 → 3547 ms inside one arm), and whichever arm ran
first collected the drift as an advantage. The ratio above survives
alternation; the 15% was the ordering.

`bt18` is unchanged, and its census says why: `gated=0 declined=0` on the
optimizing tier. Its hot method is recursive and compiles single-pass, so this
arm never engages there. A null result from a workload the change cannot reach
is not evidence in either direction, which is the reason the probe exists.

## Engagement, per collector

| collector | sites | executions |
|---|---|---|
| Generational | `gated=2 declined=0` | `inline=16,384,000 helper=0` |
| ZGC | `gated=2 declined=0` | `inline=16,384,000 helper=0` |
| G1 | `gated=0 declined=2` | — (`no-published-barrier-plan`) |
| `CRATONVM_JIT_IR_REF_STORE=0` | `gated=0 declined=2` | — (`switch-off`) |

## Levers

- `CRATONVM_JIT_IR_REF_STORE=0` — this emitter off, back to the unconditional
  helper. Separate from the single-pass `CRATONVM_JIT_GATED_REF_STORE` on
  purpose: one lever covering both could not separate "the plan is wrong" from
  "this emitter is wrong".
- `CRATONVM_NO_JIT_INLINE_PUTFIELD` — the older, broader lever; honoured here
  too, so someone who sets it to chase a lost store is not still getting inline
  stores from the tier they were least likely to look at.
- `CRATONVM_GC_JIT_REF_STORE_GATES=0` — withhold the plan entirely; both tiers
  decline.
- `CRATONVM_DBG_IR_REF_STORE_TRACE=1` — the run-time path census. A `LOCK INC`
  per store, so a diagnostic arm only.

## Gates

Run on the merged tip, not on the branch point — dev moved under this branch
twice while it was open, and one of those moves rekeyed `compact_fields` from
`pc` to `(pc, is_reference)`, which this arm had to follow.

- `regression-suite/run.sh`: **88/88** on the default collector, **88/88**
  under `-XX:+UseGenerationalGC`.
- `cratonvm-jit` 2184, `cratonvm-gc` 1795 plus its integration binaries,
  `cratonvm-types` including the regenerated flag docs.

## Residuals

- **`bt18` and every other recursive hot method still pay the full helper**,
  because they compile single-pass and the single-pass gated arm is compact-only
  — it has exactly the defect 2 that was just fixed here. Its census reads
  `gated=2` on bt18 and, if the same trace were wired there, would almost
  certainly read `inline=0`. Giving that arm the legacy shape is the next
  change, and it should carry its own dynamic census rather than inherit this
  page's conclusion.
- **The compact shape is now the rare one, not the common one.** The real
  question underneath both defects is why the TLAB fast path never consults
  `plan_object_alloc`, so that objects with a registered compact layout are
  allocated legacy. Fixing that would shrink `HashMap.Node` from 72 bytes to 56
  (see `header-shrink.md`) and make the compact arm the hot one. It is a much
  larger change and it has its own page waiting to be written.
- **A barrier-heavy A/B on a real application** is still owed. This page has a
  probe and a null result on bt18; neither is a workload.
