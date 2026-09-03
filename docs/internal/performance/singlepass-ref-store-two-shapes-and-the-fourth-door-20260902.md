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

## Fix 3 — a specialization that is dead on the default collector

The fresh-constructor arm was ordered ahead of the gated one, which is right:
a `new`-produced object needs no barrier at all, so it emits no gates. Except
that arm opens with

```rust
if !g1 && !region_bounds_are_live(self.helpers.region_bounds_addr) {
    self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);
    return;
}
```

and **only G1 and ZGC call `publish_movable_bounds`** — the generational
collector never publishes that table. So under the default collector the
specialization degrades to a full helper call on every constructor field store,
which is precisely what bt18 is made of.

The ordering is now conditional on the arm being able to run:
`fresh_ctor_first_store && region_bounds_are_live(...)`. Where the
specialization is live it still wins; where it is not, the gated arm takes the
store instead of a helper call taking it.

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

## Residual — the Generational reading

Under `-XX:+UseGenerationalGC`, bt18 reads `gated=2 declined=0` and executes
neither site, while ZGC on the same binary and workload reads `gated=6` and
136.6M executions. Inlining statistics are identical between the two
(`inline-reserve-*` match exactly), so the four inlined sites are compiled in
both cases; why they are neither gated nor declined under Generational is **not
established here**, and guessing at it would be the mistake this whole line of
work has been about. It wants its own census — the decline reasons at that door
are counted now, which is the instrument the next person needs and did not have.

That also means the throughput result above is a ZGC result. The default
collector's bt18 has not been shown to move.

## Levers

- `CRATONVM_JIT_GATED_REF_STORE=0` — the single-pass gated arm off, at every
  door, back to the pre-existing arms.
- `CRATONVM_DBG_SP_REF_STORE_TRACE=1` — the run-time path census: inline (and
  how many of those still barriered), helper, and which gate refused. A
  `LOCK INC` per store, so a diagnostic arm and never a timed one.

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
