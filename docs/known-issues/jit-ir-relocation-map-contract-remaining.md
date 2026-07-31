# IR relocation map contract — frame side done, publication side remaining

**Status: 🟡 PARTIAL.** The maps, safepoint ids and frame layout are implemented
and exercised (`92b6045e7`). `moving_young_coverage_complete` is deliberately
`false` until shadow publication lands. One step remains, specified below.

## Why the map alone is not enough

`OopMapEntry::frame_slot_offsets` is consumed by
`conservative_roots::scan_oop_slots`, which reads each slot and pushes the
**`ObjectRef` value** into the root vector. Those are *marking* roots: they keep
the object alive but give the collector no way to write the new address back
into the frame. The rewritable homes come from the shadow stack, which the
verifier reads via `published_shadow_values` and cross-checks against the frame
band (`band_has_unpublished_young_word`).

So `moving_young_coverage_complete: true` asserts *publication*, not
*enumeration*. Setting it without publishing was measured: the verifier stops
taking its cheap early-out, runs the full band scan on every live frame at every
collection, and rejects with `compiled-frame-oop-not-published` —
`ZonedDateTimeTest` went from a 302 s pass to a >1200 s timeout while still
never relocating. It was also only *safe* because the band scan caught the false
claim, which is soundness resting on the verifier catching a producer's lie.

## What remains

Mirror `x64::emit_shadow_push` / `emit_shadow_reload` in `ir_lower.rs`:

1. **Two more reserved slots**, beside the existing sp-id slot: the cached
   `*mut JvmThread` and the push's base `top` (`shadow_savebase_slot_off` in the
   single-pass backend — it makes the reload immune to an intervening unbalanced
   push, see spring-bug-10).
2. **Prologue**: call `helpers.get_current_thread`, store the result to the
   thread slot. Do it *after* the ABI parameter stores — the helper clobbers
   caller-saved registers, and by then the parameters are already in frame
   slots. Gate every use on `get_current_thread != 0`, exactly as the
   single-pass backend does; the JIT unit tests' stub helper table leaves it
   zero and dereferencing it would read stack garbage as a thread pointer.
3. **Push**, immediately before the call, over the offsets already collected
   into `frame_slot_offsets`. Safe to emit where `emit_safepoint_map` is called
   today (top of the `Op::Call` arm): R10/R11/RAX are free there because
   argument staging and the ABI register loads have not happened yet.
4. **Reload**, after the call — and this is the trap: **it must follow the store
   of the call's result to its slot.** The reload needs a scratch temp for
   frame-resident homes and the single-pass backend uses RAX, which is exactly
   where the return value is. The `Op::Call` arm has four routes
   (`emit_self_recursive_call`, `emit_direct_cross_call`,
   `emit_inline_cache_call`, generic dispatch) and each `return`s early, so the
   reload cannot simply be appended after the `match` — either give the routes a
   common tail or emit it per route after their result store.
5. Flip `moving_young_coverage_complete` to the `coverable` value already
   computed in `emit_safepoint_map`.

## Do not "simplify" by skipping the reload

Publishing values without reloading is only sound if the collector treats shadow
entries as PINNED (`CRATONVM_SHADOW_PIN`) — otherwise it relocates the object,
rewrites the shadow copy, and the frame slot keeps the stale address. Pinning
every IR-held reference would also forfeit most of the compaction the contract
exists to enable, so it is a fallback, not the design.

## What already works

With the maps in place and the flag off, `BinTreesClassic 18` at `-Xmx512m`
reports `cycles=25 coverage_fallbacks=0` and returns the HotSpot checksum
`68332206` — the moving young generation copying under live JIT frames. That
path does not depend on IR frames proving coverage; it is what dev's
`CRATONVM_MOVING_YOUNG_NO_JIT` rework unblocked. The IR contract extends the same
guarantee to frames the optimizing tier produces.
