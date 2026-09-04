# The single-pass reference store: two shapes, and a fourth door

*2026-09-02. Branch `perf/jit-singlepass-ref-store-20260902`. The residual named
by `ir-tier-ref-store-and-the-gate-that-was-a-constant-20260902.md`, which
predicted this arm had the same defect and no instrument to show it.*

## The prediction, and the number

That page ended: *"the single-pass gated arm is still compact-only — it has
exactly the defect just fixed here, and its `gated=2` on bt18 almost certainly
sits on top of `inline=0`."*

It does. The single-pass twin of the run-time path census
(`CRATONVM_DBG_SP_REF_STORE_TRACE=1`) was wired first, before any fix, and read
on `RefStoreLoopProbe` under the **default** configuration:

```
[cratonvm] compiled reference stores: gated=2 declined=0
[cratonvm]   ref-store executions: inline=0 (of which barriered=0) helper=16380000
[cratonvm]     ref-store bail receiver-not-compact: 16380000
```

Sixteen million executions, not one of them inline, every one refused at the
per-object compactness test — and a compile-time census reporting the arm as
fully engaged.

## Fix 1 — both store shapes

`init_object_header`, the TLAB fast path serving nearly every allocation for
the interpreter and `jit_new_object` alike, writes a LEGACY header
unconditionally — no `GC_FLAG_COMPACT` — whatever layout the class has
registered, because it never consults `plan_object_alloc`. A store gated on
that bit therefore stores inline almost never.

`emit_gated_compact_ref_putfield` now emits both shapes and picks per OBJECT,
exactly as the inline `getfield` read has since 2026-08-18 and as
`jit_putfield_object` always has. The legacy shape is the uniform 16-byte
`Value` cell — tag qword then pointer payload — and the `field_index <
num_slots` check already present is what makes its stride addressable.

The regression test **executes** the compiled body rather than inspecting it:
it compiles `this.f = v`, runs it against a legacy fake object and asserts the
cell bytes and that no helper was called, then against a compact one, then
against an old-generation one where the collector's own write barrier must
still run. Checked against the old code, where it fails with exactly the
predicted `(1, 0)` helper/barrier pair.

## Fix 2 — the fourth door

With that fixed, `bt18` still read `gated=2` with **no execution line at all**:
two cold top-level sites, and its hot stores somewhere else entirely.

They were in the **inlined-callee `putfield` arm** (`x64/inlining.rs`), which
never tried the gated sequence. It has its own three-way branch — a
fresh-constructor specialization, a general body arm, and the helper — and none
of them consulted the collector's published barrier plan. Worse, none called
`note_ungated_ref_store()`, so those sites did not appear in the census even as
declines. The census could not have shown the gap.

This is the repo's recurring shape: `three-compile-doors-try-compile-is-not-the-only-one`,
`a-thin-native-bind-needs-four-doors`, `a-fast-dispatch-arm-bypassed-the-cache-invalidation`.
A gate wired into one door is a gate for one door.

The gated arm is now tried at this door too, and every fall-through is counted.

## Fix 3 — a specialization that is dead on ZGC

**Corrected 2026-09-02, after this page first shipped saying the opposite.**
The original text claimed the fresh-constructor arm was dead on the *default*
collector because "only G1 and ZGC call `publish_movable_bounds`". That is the
wrong table. `publish_movable_bounds` writes `MOVABLE_BOUNDS`;
`region_bounds_are_live` reads `JIT_REGION_BOUNDS`, and the only writer of
`JIT_REGION_BOUNDS` is `GenerationalHeap::publish_region_bounds` — as
`gen_heap.rs` says in as many words: *"the two tables only diverge on G1
(read-only publish) and ZGC (neither)"*.

So it is the other way round. The fresh-constructor arm opens with

```rust
if !g1 && !region_bounds_are_live(self.helpers.region_bounds_addr) {
    self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);
    return;
}
```

and those bounds are live **only** under the generational collector. Under ZGC
the specialization degrades to a full helper call on every constructor field
store, which is precisely what bt18 is made of — and that, not anything about
the default collector, is what the 10-of-10 measurement below is measuring.

The ordering is now conditional on the arm being able to run:
`fresh_ctor_first_store && region_bounds_are_live(...)`. Where the
specialization is live — the generational collector — it still wins; where it
is not, the gated arm takes the store instead of a helper call taking it.

Two independent confirmations, since a claim this page got backwards once
deserves them: the source above, and the emitted code. `bottomUpTree`
disassembled under `-XX:+UseGenerationalGC` contains the fresh-constructor
arm's `test byte [rax+0Fh], 4` (a direct memory-operand test), while the same
method under ZGC contains the gated arm's `movzx ecx, byte [rax+0Fh]` /
`test cl, 4`.

## Engagement

`bt18`, one binary, per collector:

| collector | sites | executions |
|---|---|---|
| ZGC | `gated=6 declined=0` | `inline=136,661,550 (barriered=0) helper=0` |
| G1 | `gated=0 declined=6` | — (no published plan; the interlock holds) |
| Generational | `gated=2 declined=0` | none — see the residual below |

136.6 million stores that were previously helper calls, on a workload that
reported `gated=2` and zero executions before this change.

## The measurement

`bt18` under ZGC, one binary, order alternated by round, `CRATONVM_JIT_GATED_REF_STORE=0`
as the control:

| arm | median wall |
|---|---|
| gated arm on | **11.86 s** |
| `CRATONVM_JIT_GATED_REF_STORE=0` | 12.48 s |

**10 of 10 rounds** faster with the arm on, ~5% at the median. All twenty runs
printed the same tree checksum `[68332206]`, so every arm did the same work —
which the previous, sloppier version of this harness could not show, because it
digested a line carrying bt18's own elapsed time and therefore differed every
run for reasons having nothing to do with the arms.

5% is a smaller number than the optimizing tier's 3.2x, and it should be: bt18
spends most of its time allocating and recursing, and the barrier was never the
majority of it. The 136.6 million eliminated calls are the fact; the 5% is what
they are worth in this workload.

## The Generational reading — resolved, and it was not a defect

When this page first shipped it left an open residual: under
`-XX:+UseGenerationalGC`, bt18 read `gated=2 declined=0` and executed neither
site, while ZGC on the same binary read `gated=6` and 136.6M executions.

It is not a hole. Chased with the instruments rather than guessed at:

1. `CRATONVM_DBG=jit-field-sites` shows **identical site sets** on both
   collectors — four `putfield/inlined` in `bottomUpTree`, two `putfield` in
   `Node.<init>`. Inlining is the same.
2. The disassembly places the two gated sequences under Generational inside
   `Node.<init>`, not `bottomUpTree`. `Node.<init>` is compiled as a standalone
   body and then never called, because every call to it is inlined — hence
   `gated=2` with zero executions.
3. `bottomUpTree`'s four hot stores under Generational take the
   **fresh-constructor arm**, which is live there and only there (see Fix 3).
   That arm stores inline with no barrier and no helper call at all.

So under the default collector those stores were already on the cheapest path
available. What was missing was not codegen but a census: neither the
fresh-constructor arm nor the general body arm counted anything, at compile
time or at run time, so a workload served entirely by them read as `gated=2`
and nothing else — indistinguishable from a hole.

Both arms are now traced under `CRATONVM_DBG_SP_REF_STORE_TRACE=1`, and the
reading resolves:

`bt16`, one binary, one workload:

| collector | gated sites | gated executions | fresh-ctor executions |
|---|---|---|---|
| Generational | 2 (never called) | 0 | **29,966,944** |
| ZGC | 6 | **29,969,414** | 0 (arm not live) |
| G1 | 0 (6 declined) | — | 0 |

The same stores, within 0.01% of each other, served by a different arm on each
collector. That agreement is the cross-check: two independent counters on two
configurations arriving at the same workload-determined number is much harder
to fake than either one alone.

The throughput result above remains a ZGC result, and now for a stated reason:
under Generational the stores this change would have moved were not on the
helper to begin with.

## Postscript, 2026-09-04: the premise inverted

This page argued for the legacy store shape on the grounds that compact
receivers were rare — `init_object_header` wrote legacy headers unconditionally,
so a compact-only arm fired zero times out of 16,380,000.

The compact TLAB shape became the DEFAULT on 2026-09-04, and a per-shape census
added the same day says the situation is now exactly reversed:

| configuration | workload | compact | legacy |
|---|---|---|---|
| default | `RefStoreLoopProbe` | **16,384,000** | 0 |
| default | H2 `TestIntPerfectHash` | **199** | 0 |
| `CRATONVM_COMPACT_TLAB_ALLOC=0` | `RefStoreLoopProbe` | 0 | **16,384,000** |

So the legacy arm now carries none of the workload on either a probe or a real
application. It has not stopped earning its place — it is what makes
`CRATONVM_COMPACT_TLAB_ALLOC=0` a working revert lever, and it still serves any
class with no registered layout — but it is the fallback now, not the hot path.
Anyone reading the argument above should read it as the history of why the arm
exists, not as a description of what it does today.

Read the pair, never one number: `compact=N legacy=0` under the default and
`compact=0 legacy=N` under the kill switch is what says the two-shape store is
picking per object rather than always taking one branch.

## Levers

- `CRATONVM_JIT_GATED_REF_STORE=0` — the single-pass gated arm off, at every
  door, back to the pre-existing arms.
- `CRATONVM_DBG_SP_REF_STORE_TRACE=1` — the run-time path census: inline (and
  how many of those still barriered), helper, which gate refused, and the
  executions of the two non-gated inline arms. A `LOCK INC` per store, so a
  diagnostic arm and never a timed one.
- `CRATONVM_DBG=jit-field-sites` with `CRATONVM_DBG_JIT_FIELD_SITES=<filter>` —
  which field sites a method compiled, and whether each is `putfield` or
  `putfield/inlined`. This is what settled the Generational question above.

## Gates

`regression-suite/run.sh` 88/88 on the default collector, 88/88 under
`-XX:+UseGenerationalGC`, 88/88 under `-XX:+UseG1GC`; `cratonvm-jit` 2185,
`cratonvm-gc`, `cratonvm-types` with the flag docs regenerated.

The new legacy emission site bakes `HEADER_SIZE + field_index * SLOT_SIZE` into
machine code and **neither automated layout tripwire sees it** — `x64.rs`'s
scans for the `<CONST> as <ty>` cast form, and the `layout_constant_inventory`
covers only `lib.rs` and `ir_lower.rs`. It is listed by hand in
`header-shrink.md` §6.6 and `layout-constant-hazards.md` §3, and extending the
inventory to `x64/objects.rs` is a worthwhile separate change.
