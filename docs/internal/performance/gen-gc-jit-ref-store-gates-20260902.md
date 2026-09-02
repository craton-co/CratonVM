# The generational collector can publish a reference-store barrier plan now — and it buys nothing yet, for a reason worth writing down

Slug: `gen-gc-jit-ref-store-gates` · 2026-09-02
Closes the residual `gen-gc-minor-pause-20260902.md` §F5 left open, and
replaces it with a sharper one.

---

## VERDICT

**The gap was real and is closed.** Under `-XX:+UseGenerationalGC` every
compiled reference store paid a full `jit_putfield_object` call, while the
identical sites under `-XX:+UseZGC` took an inline gated sequence. Same
workload, same binary, same two sites:

| collector | before | after |
|---|---|---|
| `-XX:+UseGenerationalGC` | `gated=0 declined=2` | **`gated=2 declined=0`** |
| `-XX:+UseZGC` | `gated=2 declined=0` | `gated=2 declined=0` (unchanged) |

**And it buys no measured throughput**, on anything measured here. That is not
a disappointing result to be buried; it is the finding:

> The barrier plan is **inert on the optimizing tier — for BOTH collectors**.
> `ir_lower.rs` lowers every reference store to `jit_putfield_object`
> unconditionally (its own COV-03 comment says so), and a hot loop is compiled
> by that tier. So the plan only ever reaches single-pass code, and the sites
> it reaches on the probes here are not hot enough to move a wall clock.

---

## 1. Why the generational collector could not publish a plan

v10 gave compiled code three gate bytes and one way to rule the post barrier
out: an unsigned FLOOR compared against the receiver's `GC_FLAGS_BYTE_OFFSET`
byte, which packs `gc_age` in bits 4..7 and the GC flags in bits 0..3. A floor
of `age << 4` is an exact test of `gc_age < age`, because the flags nibble is
at most 15 and cannot carry.

That shape can only say *young enough*. The generational post barrier asks a
different question — **is the receiver in the old generation** — and answers it
with `GC_FLAG_OLD_GEN`, a bit in the very nibble the floor treats as noise. The
two do not order:

| receiver | flags byte | needs a card? |
|---|---|---|
| allocated straight into old gen, `gc_age == 0` | `0x01` | **yes** |
| young, survived three collections | `0x30` | no |

The receiver that needs a card sorts **below** the one that does not. Any single
unsigned threshold would have told compiled code to skip the card on exactly
the wrong receivers — a lost old→young edge, which surfaces as a reclaimed
still-live object somewhere else entirely. `no_age_floor_can_separate_an_old_gen_receiver_from_a_young_one`
pins those two numbers.

So the fix is not a different floor, it is a different SHAPE:
`JitRefStoreGates::post_skip_mask`, a flags mask (helper ABI v12). A receiver
carrying none of its bits provably needs no post barrier, which compiled code
tests with one `test r8, imm8`. A publisher supplies the floor **or** the mask,
never both — `ref_store_gates` refuses a plan that offers both, because for a
mask publisher the floor is not merely redundant but wrong.

Two things that had to be got right and are asserted rather than assumed:

* **The floor's ADDRESS is non-zero whether or not the floor is in use.**
  `jit_ref_store_gate_addrs` withholds it when the value is 0, or the emitter
  sees both shapes published and declines the whole fast path. This was a live
  bug during development: the plan published correctly, engagement stayed at
  `gated=0`, and the cause was a `static`'s address being truthy.
* **The SATB mirror counts markers, it does not set a boolean.**
  `ConcurrentGcState` is shared by the generational collector AND G1, and a
  process can hold several heaps at once. A boolean would let one heap leaving
  its mark phase clear the gate while another is still marking, and compiled
  code would then skip a live SATB pre-barrier. `set_phase` arms BEFORE the
  phase becomes observable and disarms AFTER it stops being, so the gate is
  armed for a superset of the interval it mirrors.

## 2. What it is worth: nothing measured, and why

`bench/BinTreesClassic 18` at `-Xmx2g`, one binary, `CRATONVM_GC_JIT_REF_STORE_GATES=0`
as the only difference, four interleaved rounds:

| arm | wall (r1 / r2 / r3 / r4) |
|---|---|
| gates on | 2743 / 2862 / 2855 / 2742 ms |
| gates off | 2756 / 3381 / 2800 / 2614 ms |

No separation, and the sign flips between rounds. The reason is not that the
gates fail to engage — `gated=2 declined=0` on that very run — but that those
two sites are not where the time goes.

`bench/RefStoreLoopProbe` was written to be unambiguous: a flat counted loop
whose body is almost nothing but reference stores into young receivers, no
allocation in the steady state. It reports **`gated=0 declined=0`** — the
emitter is never reached at all — and `[ir] admission ... admitted to the
optimizing pipeline` says why. Two facts fall out of it:

* **The optimizing tier lowers every reference store to the helper**, for every
  collector. `ir_lower.rs`'s COV-03 comment states the rule and the reason: it
  has no compact field offset to store through, so it cannot prove the premises
  the single-pass emitter proves. **`-XX:+UseZGC` reports `gated=0 declined=0`
  on the same probe**, which is the cleanest possible evidence that this is not
  a generational problem — v10's plan has never helped IR-compiled code either.
* **The single-pass gated arm needs a statically-known compact receiver.** It
  fires on `BinTreesClassic`'s two sites, where the receiver's class is known
  at the store, and not on a receiver loaded out of an array.

So the reachable population is "reference stores in single-pass code with a
statically-known compact receiver", and on these probes that is two sites doing
too little work to measure.

## 3. What this change is, then

**Enabling, not a win.** It removes an asymmetry that had no reason to exist —
the same store shape gated under one collector and not the other — and it
supplies the mask machinery any future work needs. It is default-on because a
declined plan and a published one produce the same program, and it ships with
`CRATONVM_GC_JIT_REF_STORE_GATES=0` as a one-run bisection lever.

It is **not** filed as a throughput improvement, and nothing here should be
quoted as one.

## 4. The residual, which is now the interesting one

**Give the optimizing tier a gated reference store.** That is where hot loops
are compiled, and today every one of their reference stores is a full helper
call under every collector. It needs what COV-03 says it needs: the compact
field offset plumbed into the IR lowering so the store can be emitted inline,
after which the same three gates — and the mask this change adds — apply
unchanged.

Sizing it honestly: that is a change to a register-allocated backend, in the
one place whose failure mode is a silently missing barrier, and it should carry
its own measurement on a probe like `RefStoreLoopProbe` rather than being
argued from this page.

## Not established

* **No throughput claim.** §2.
* The probes here are single-host on a shared 32-core Windows box; what §2
  establishes is the absence of a separation, not a precise cost.
* `RefStoreLoopProbe`'s value as a barrier measurement is currently latent: it
  measures the tier that has no gated path. It is committed because it is the
  probe the residual in §4 needs, and because it is what showed the plan to be
  inert there.
