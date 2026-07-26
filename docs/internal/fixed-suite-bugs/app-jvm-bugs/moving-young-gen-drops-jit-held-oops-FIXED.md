# `CRATONVM_MOVING_YOUNG` silently corrupted the heap when the JIT was enabled

| | |
|---|---|
| **Status** | ✅ **FIXED** 2026-07-26 on `fix/moving-young-jit-oops-20260726` (merged to `dev`). bt18 returns `68332206` on every run with `CRATONVM_MOVING_YOUNG=1` and the JIT **on**, at `-Xmx8g` and at heaps small enough to run 25 moving cycles. |
| **Root cause** | Five single-pass codegen sites pushed an object reference onto the simulated operand stack without tagging it (`stack_oop_marks`), so it was published to neither the precise oop map nor the shadow stack — while the safepoint still certified `moving_young_coverage_complete`. |
| **Originally captured** | 2026-07-25, branch `arch/tiers-1-3-20260725`. Host Azure `20.83.144.174`. |
| **Fixed in** | `jit/src/x64.rs` (the five tag sites), plus verifier/diagnostic work in `jit/src/lib.rs`, `vm/src/jit/conservative_roots.rs`, `gc/src/vm_heap.rs`. |

## Original symptom

`CRATONVM_MOVING_YOUNG=1` makes the young generation a copying collector
instead of the default non-moving sweep. Under enough GC pressure it lost live
references and the program computed a **wrong result that varied between
runs** — `bench/BinTreesClassic.java` at depth 18, whose correct checksum is
the documented constant `68332206`, returned `68310826` / `68310832` /
`68311064`. `CRATONVM_DISABLE_JIT=1` made it correct; depth 14 was clean;
`CRATONVM_ALLOW_MOVING_YOUNG=1` (since deleted) made it 14× worse.

## Root cause

An entry on the JIT's simulated operand stack carries an oop mark
(`Compiler::stack_oop_marks`). Everything downstream keys off it:

* `collect_live_oop_homes` publishes only **marked** entries onto the shadow
  stack — the one channel through which a moving collector can *rewrite* a
  compiled frame's reference;
* `emit_oop_map_for_safepoint` records only **marked** entries' frame slots;
* `moving_young_safepoint_coverage_complete` certifies the safepoint by
  checking that **marked** entries have frame/register homes — an *unmarked*
  oop is invisible to it, so an untagged reference does not merely go
  unpublished, it actively causes the frame to claim complete coverage.

Under `CRATONVM_MOVING_YOUNG`, `vm/src/memory/roots.rs` suppresses the
conservative frame scan for a certified frame. So an untagged reference was
neither marked nor rewritten, and the Cheney copy stranded or reclaimed
everything hanging off it.

Five sites pushed a reference and left the default `false` mark:

| # | Site (`jit/src/x64.rs`) | What it pushes |
|---|---|---|
| 1 | direct **self-recursive `invokestatic`** | the recursive call's result |
| 2 | direct **`invokevirtual`/`invokespecial`/`invokeinterface`** to a compiled callee | the callee's `L`/`[` return |
| 3 | **inlined callee return** (`areturn` → `pop_to_rax`/`push_from_rax`) | the inlined method's result |
| 4 | **`getfield` inside an inlined callee** | a reference field's value |
| 5 | **`getfield` on a scalar-replaced object** | a reference field's value |

Site 1 is the measured bt18 instance. `BinTreesClassic.bottomUpTree` is

```java
return new Node(bottomUpTree(depth - 1), bottomUpTree(depth - 1));
```

so the **first** recursive call's result — an entire subtree — sits on the
operand stack across the **second** recursive call, which is a GC-capable
safepoint. Confirmed at compile time: `CRATONVM_DBG_SHADOW2=1` showed
`pc=27 stack=[Frame(72), Frame(80), Frame(88)] marks=[true, true, false]`,
with `homes=[Frame(72), Frame(80)]` — the subtree at `Frame(88)` published
nowhere.

The three storage classes hypothesised in
`docs/internal/arch-2026-07-26/moving-young-corruption-rootcause.md` §3
(scalar-replacement fields, LICM hoist slots, the blind GPR spill) are **not**
what corrupted bt18: `bottomUpTree`'s published `FrameLayout` has
`scalar_lo: 0, scalar_hi: 0, ref_hoist_lo: 0, ref_hoist_hi: 0`. Site 5 above
does close the scalar-replacement case, but as an audit result, not as the
measured producer.

Why the default collector never saw this: the conservative frame scan reads
every word of the frame and pins whatever looks like an object, so an untagged
reference in a frame slot is still *marked*. It is only un-**rewritable**, and
nothing relocates on the default path.

## Fix

1. **`jit/src/x64.rs` — tag the five sites.** Each now calls
   `mark_top_as_oop()` when the descriptor says `L` or `[`. The self-recursive
   site additionally **fails closed**: when the method descriptor is
   unavailable (the legacy `compile()` test wrapper passes an empty
   `method_key`) it clears `stack_oop_marks_exact`, so the frame stops
   certifying moving-young coverage rather than guessing.

2. **`jit/src/lib.rs` — `CompiledMethod::frame_layout`** (new `FrameLayout`).
   The compiler now publishes the frame's storage-class partition (Java
   locals, LICM ref/arith hoist slots, scalar-replacement fields, operand
   spill, callee-saved GPR/XMM images, the per-safepoint blind GPR spill).
   Also `method_label` and `shadow_savebase_slot_off`, for frame-level
   diagnostics.

3. **`jit/src/lib.rs` — `OopMapEntry::live_frame_hi`.** The operand-spill
   cursor at each safepoint. The spill reserve is sized for `max_stack` and
   reclaimed by *moving a cursor*, never by clearing, so a slot above the
   cursor keeps whatever the deepest earlier operand stack left there — in
   allocation-heavy code, a stale object pointer.

4. **`vm/src/jit/conservative_roots.rs` — the frame-band verifier is now
   precise.** It skips register IMAGES and outgoing args (everything at or
   beyond `callee_saved_lo`) and reclaimed spill slots (above
   `live_frame_hi`), and verifies exactly the storage a compiled frame
   resumes from. Before this it reported a phantom miss on *every* collection,
   which is why the pre-fix tree measured "correct" while running **zero**
   moving cycles.

5. **`gc/src/vm_heap.rs` — the GC summary reports both numbers.**
   `[GC] moving_young: cycles=N coverage_fallbacks=M` plus a per-reason
   histogram, so "the checksum is right" can never again be mistaken for
   "moving-young works" when it never engaged (the 2026-07-01 mistake).

`CRATONVM_MOVING_YOUNG_BAND_DBG=1` names the frame region of every word the
verifier rejects; `CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY=1` drops the verifier
entirely, which is how "is the codegen model actually sufficient?" is asked.
The second is a measurement instrument, not a supported configuration.

## Validation

All on the Azure host, real JDK 25, JIT on, `bench/BinTreesClassic.java`.
`cycles` is `gc_quiescence::moving_young_cycle_count()` — moving (Cheney)
young collections that ran **while a JIT frame was live**.

| Configuration | Checksum | Cycles | Fallbacks |
|---|---|---|---|
| default (non-moving young), `-Xmx8g` ×3 | `68332206` ✅ | — | — |
| `MOVING_YOUNG=1`, `-Xmx8g` ×3 | `68332206` ✅ | 1 | 0 |
| `MOVING_YOUNG=1`, `-Xmx2g` | `68332206` ✅ | 6 | 0 |
| `MOVING_YOUNG=1`, `-Xmx1g` ×5 | `68332206` ✅ | 12 | 0 |
| `MOVING_YOUNG=1`, `-Xmx512m` ×2 | `68332206` ✅ | 25 | 0 |
| `MOVING_YOUNG=1`, bt16 | `14985902` ✅ | 0 | 0 |
| `MOVING_YOUNG=1`, bt14 | `3222190` ✅ | 0 | 0 |

The load-bearing control, on the **pre-fix** tree with the band verifier
disabled so the codegen's coverage bit is trusted (i.e. the state the fix had
to repair):

| Tree | Checksum ×3 | Cycles |
|---|---|---|
| pre-fix, `NO_BAND_VERIFY=1` | `68304634`, `68300790`, `68301020` ❌ varies | 1 |
| post-fix, `NO_BAND_VERIFY=1` | `68332206` ×3 ✅ | 1 |

One moving cycle was enough to lose ~28 000 nodes. That the same
configuration is now correct is the proof that the codegen model — not the
runtime backstop — is what was repaired.

Default path unchanged: `QuickBenchLong2` (all 5 kernels), `CratonBench`,
`HashMapOnly` and `StringRegexOnly` produce byte-identical results with and
without `CRATONVM_MOVING_YOUNG=1`.

Unit tests: `cargo test --release -p cratonvm-gc --lib` 864/864;
`-p cratonvm-jit --lib` 1013/1016 (the 3 failures are pre-existing stale
AArch64 `idiv`/`irem` tests from commit `82a9d08fc`, unrelated);
`-p cratonvm-vm --lib` 2405 passed, 18 failed (all `runtime::lock_order`,
which require `debug_assertions` and fail in any `--release` run).

New regression tests:

* `jit::x64::tests::self_recursive_reference_return_is_published_as_an_oop`
  — the exact defect: fails on the pre-fix tree.
* `..::self_recursive_primitive_return_is_not_published_as_an_oop` — the
  other direction: an `int` result must NOT be tagged, or the collector would
  relocate whatever its bit pattern names.
* `..::self_recursive_return_without_a_descriptor_fails_closed`.
* `jit::conservative_roots::tests::frame_band_scan_skips_register_images`
  and `..::frame_band_scan_ignores_reclaimed_spill_slots` — the two
  false-positive floors that kept moving-young inert.

## Residuals (open, tracked separately)

1. **Throughput.** A correct moving young gen is *slower* than the default on
   bt18, and by more than the original doc measured: at `-Xmx8g` on a busy
   16-core host, `MOVING_YOUNG=1` ran ~19 s against ~6–8 s for the default.
   Two separable costs — the codegen overhead moving-young forces on every
   compiled method (shadow push/reload plus the full-GPR safepoint spill;
   ~7–8 s even on runs that executed **zero** moving cycles) and the Cheney
   copy itself. bt18 is the worst case for a copying collector (a very large
   live set), so this measurement alone does not settle the design question —
   but it does mean **`DEFAULT_MOVING_YOUNG` must not be flipped on the
   strength of this fix**. See
   `docs/feature-designs/default-moving-young-gen.md`.

2. **Over-strict cross-thread proof.** `refresh_moving_young_coverage_for_collection`
   still treats any cycle with a peer thread in JIT as unproven
   (`CROSS_THREAD_JIT_PEER`), so moving-young engages only when the
   initiator is the sole thread in compiled code. Specified in
   `docs/internal/arch-2026-07-26/moving-young-precise-roots.md` §"Remaining
   work" item 4.

3. **A5 probe over-detection.** `native_stack_has_jit_frame` is a raw-word
   scan and can fabricate a frame from a cached function pointer, diverting a
   cycle that did not need it. Same doc, item 3b.

Neither (2) nor (3) is a correctness risk — both only cost compaction.

## Related

- `docs/internal/arch-2026-07-26/moving-young-precise-roots.md` — the gate
  unification and the proof-obligation audit this built on.
- `docs/internal/arch-2026-07-26/moving-young-corruption-rootcause.md` — the
  frame-band verifier, and the three-storage-class hypothesis that this
  investigation refuted for bt18.
- `docs/feature-designs/default-moving-young-gen.md`.
- `docs/feature-designs/concurrent-gc-maturation.md`.
