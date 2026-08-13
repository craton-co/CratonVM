# Phi resolution in `ir_lower`: the parallel copy and its scratch word

Scope: the "phi web on a CFG edge is a *parallel* copy" item of
the C2 review, as it lands in the optimizing
backend.

Subject: `jit/src/ir_lower.rs` — `Lowerer::emit_phi_copies`,
`Lowerer::emit_copy_op`, `phi_copy_sequence`, `frame_word_loc` /
`frame_word_off`, and the frame reservation in `Lowerer::new`. Companion to
`docs/jit/linear-scan-regalloc.md`, which owns the algorithm
(`regalloc::resolve_parallel_copy`) and its `CopyOp` vocabulary.

---

## 0. Status in one paragraph

`ir_lower` now routes every edge's phi copies through
`regalloc::resolve_parallel_copy` and emits the resulting `Move` / `Save` /
`Restore` sequence. Cycles — the swap loop, and every other permutation of
loop-carried values — are broken with **one reserved frame word**,
`Lowerer::phi_copy_scratch_slot_off`, because RAX is already the *move*
temporary and cannot also be the cycle parking spot. The intermediate
`UnsupportedShape("cyclic phi parallel copy")` refusal is **gone**: a
swap-shaped loop compiles, and compiles correctly, instead of dropping a tier.

---

## 1. What was wrong

`prealloc_phi_slots` gives every `Op::Phi` its own frame word. The copies one
predecessor performs on its outgoing edge are therefore an assignment over
*distinct* words, and — this is the part that was missed — a **simultaneous**
one: every source must be read as it was before any destination is written.

A loop that swaps two locals makes each header phi the other's back-edge value:

```java
for (…) { int t = a; a = b; b = t; }        // or, with no temp at all:
                                            // iload_1; iload_0; istore_1; istore_0
```

```
slot(a') ← slot(b)
slot(b') ← slot(a)
```

Emitted in gather order through RAX, the second copy reads the word the first
just overwrote and **both** locals end up holding `b`. That is wrong code, not a
missed optimisation, and it is invisible to any assertion about the copy *list*
— only executing the emitted bytes shows it.

An intermediate state of the fix ordered the copies topologically (readers
first) and, when only cycles remained, refused the compile with
`BailoutReason::UnsupportedShape("cyclic phi parallel copy")`. Correct, but it
dropped every swap-shaped loop to a lower tier.

## 2. The scratch word

`Lowerer::new` reserves **five** bookkeeping words between the locals (plus the
optional context slot) and the spill band, at `[rbp - (base + k) * 8]`:

| k | word |
|---|------|
| 0 | safepoint id (`sp_id_slot_off`) |
| 1 | cached `*mut JvmThread` (`shadow_thread_slot_off`) |
| 2 | shadow save-base (`shadow_savebase_slot_off`) |
| 3 | shadow save-top (`shadow_savetop_slot_off`) |
| 4 | **phi parallel-copy scratch** (`phi_copy_scratch_slot_off`) |

`first_spill` is `(base + 5) * 8`, one word higher than it used to be. Three
properties follow, and all three are asserted in
`the_phi_scratch_word_is_reserved_below_the_spill_band`:

* **No collision with a coloured slot.** `alloc_slot_checked` places a value at
  `first_spill + colour * 8`; the scratch is strictly below `first_spill`, so
  no colour `plan_slots` hands out can land on it. `verify_slot_colouring` and
  `verify_data_locations` see an unchanged spill band, and the published
  `FrameLayout` (`locals_hi` / `spill_lo` = `first_spill`) simply counts the
  scratch as one more reserved local word, exactly as it counts the other four.
* **`estimate_frame_bytes` still equals the frame that gets built.**
  `Lowerer::new` *sizes* the frame by calling `estimate_frame_bytes`, so the two
  cannot drift; the `debug_assert_eq!` against the hand-written layout sum is
  what pins the mirror, and its `bookkeeping_size` term moved from `8 * 4` to
  `8 * 5` in lockstep with the estimator's `bookkeeping`. The frame grows by 8
  bytes (16 after alignment, half the time) per compiled method.
* **The spill band still fits.** The highest spill offset is
  `first_spill + (slots - 1) * 8`; the cap is `frame_size - shadow - reserve -
  args`. Both moved by the same 8 bytes, so the margin is what it always was.

### Why not a register, and why not zeroed

RAX is the move temporary — every `Move` is `load_to_rax; store_rax`, so RAX is
dead the instant the next move begins. Breaking a cycle needs a location that
*survives* an intervening move. A second scratch register would work but this
backend deliberately keeps every value in a frame word and reserves no register
across an edge; one frame word is the cheap, local answer.

The word is **not** zeroed in the prologue and is **never named by an oop map**.
A `Save` always precedes its matching `Restore` inside one straight-line copy
sequence — no branch, no call, no safepoint in between — so it is never read
before it is written, and never holds a live reference across a point the
collector can observe. (Contrast `shadow_thread_slot_off`, which *is* zeroed,
because its consumers are null-guarded on a value the prologue may skip
writing.)

## 3. The `ValueLoc` view

`resolve_parallel_copy` speaks `regalloc::ValueLoc`. This backend's locations are
rbp-relative byte offsets, so `frame_word_loc` / `frame_word_off` convert:

```rust
ValueLoc::Slot(off as u32)          // off is the offset, NOT off / 8
```

The resolver only ever **compares** locations — it never dereferences one, and
the scratch is deliberately unnamed in `CopyOp` — so the payload has to be an
injective token for "this frame word" and nothing more. The byte offset is
exactly that. Dividing by 8 would be a hazard rather than a tidiness: it maps two
distinct offsets onto one location the moment the offsets ever stop being
8-aligned, and `resolve_parallel_copy` **drops** a copy whose destination and
source are the same location — so the aliasing would silently delete a real
move, the same class of wrong-code bug this change removes.

`ValueLoc::Reg` is unreachable here and is refused (`BailoutReason::Internal`)
rather than mis-emitted as an offset.

## 4. Cost

| edge shape | ops | scratch touched |
|---|---|---|
| acyclic chain of *n* copies | *n* moves | no |
| self-copy (`d == s`) | 0 | no |
| 2-cycle | save, move, restore | yes |
| 3-cycle | save, 2 moves, restore | yes |
| *k* disjoint cycles | *k* × (save … restore) | yes, one at a time |

The acyclic majority — every merge that is not a permutation of live values —
pays exactly the one load + one store per copy it always paid.

**One word is enough for any number of cycles.** `resolve_parallel_copy`
prefers a ready destination on every pass, and breaking a cycle immediately
makes one entry ready, so the `Restore` of cycle *i* is always emitted before
the `Save` of cycle *i + 1*. That is an invariant of the resolver, but the frame
reservation here is what *depends* on it, so
`disjoint_phi_cycles_never_nest_their_saves` pins it from this side.

## 5. Behaviour change to be aware of

`resolve_parallel_copy` rejects a web that writes one destination twice with two
different sources (`BailoutReason::Internal`, "a parallel copy writes one
destination twice"). The old sequential emitter accepted that shape and let the
last write win — which is wrong whenever the two entries came from two *different*
edges of the same predecessor, since the edge actually taken decides the value.
The new behaviour is a refused compile (fall back a tier), i.e. fail-closed. An
identical pair listed twice is idempotent and is still accepted.

## 6. Tests

`jit/src/ir_lower.rs`, `mod tests`, section "Phi parallel copy".

The cycle cases **execute the emitted bytes**: `parallel_copy_harness` builds a
real `Lowerer` frame (so the offsets, the scratch reservation and `frame_size`
are production's), hand-rolls a minimal prologue/epilogue around the real
sequencer and the real emitter, seeds the named words plus the scratch with a
sentinel, and runs the result.

* `a_two_element_phi_cycle_really_swaps_the_two_frame_words` — the wrong-code
  witness; the old emitter left `b` in both words.
* `a_three_element_phi_cycle_rotates_the_frame_words` — 4 ops.
* `an_acyclic_phi_chain_emits_the_minimal_moves_and_no_scratch` — exact op list,
  and the scratch still holds its sentinel afterwards.
* `a_self_copy_emits_nothing`.
* `disjoint_phi_cycles_never_nest_their_saves` — the one-word invariant.
* `a_register_location_is_refused_rather_than_emitted_as_an_offset`.
* `the_phi_scratch_word_is_reserved_below_the_spill_band` — the five words are
  contiguous, no colour collides, and `estimate_frame_bytes` still equals the
  built `frame_size` (asserted in release too, not only via the
  `debug_assert_eq!`).
* `the_swap_loop_compiles_and_swaps` — end to end from bytecode. Returns
  `a - b`, so `0` is the readout for *either* duplication bug and a `None`
  compile is the readout for the refusal coming back.

## 7. Reconcile

`docs/jit/linear-scan-regalloc.md:215-232` carries a block titled
*"Reconcile — this is a live bug in `ir_lower.rs`, not only a design note."*
It is now stale: the bug is fixed. That file is owned elsewhere; the block
should be replaced with a pointer here, e.g.

> **Consumed.** `ir_lower.rs` routes its phi copies through
> `resolve_parallel_copy` and breaks cycles with one reserved frame word
> (`Lowerer::phi_copy_scratch_slot_off`). See `docs/jit/phi-parallel-copy.md`.

`regalloc::ValueLoc::Slot`'s doc says the payload is "an 8-byte word index, as
`SlotPlan::node_color` numbers them". `ir_lower` uses the rbp-relative byte
offset instead (§3). Nothing in `resolve_parallel_copy` interprets the payload,
so both are valid; the doc could be relaxed to "an opaque per-word token,
injective within one copy" to stop reading as a constraint.
