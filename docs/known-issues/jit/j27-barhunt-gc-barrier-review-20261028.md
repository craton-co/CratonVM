> **j27 wave 8 (lane W8REV, 2026-10-28): REVIEWED (the "not reviewed in depth" row closed by reading; no defect, no code change), PENDING HOST VERIFICATION.** The optimizing tier's `System.arraycopy` site binds `jit_system_arraycopy_direct` (`vm/src/jit/helpers/dispatch.rs`; `ir_arraycopy_entry`), whose reference arm is `try_reference_arraycopy_pair`: a CHECKED copy stores through `copy_reference_slots_raw_checked` or `publish_overwritten_array_element` (SATB) + `set_array_element` (card) per element, one load per proof; an UNCHECKED copy takes the raw word arm only when the collector admits it (`copy_reference_slots_raw`: ZGC with no mark and no armed load barrier, then `note_ref_store(dst)` = an object card the element-card consumer converts, per the G2 notes in `gc/src/zgc.rs`; G1 / Gen with no SATB window, no humongous array, wide slots, then the per-stored-reference post obligation), else the per-element accessors in memmove order. `Arrays.copyOf` / `copyOfRange` in the IR tier is the spliced JDK body over a fresh `Array.newArray` result plus that same helper (the inline arraycopy arm is opt-in and byte-only, so no reference barrier is involved). The `ArrayList` intrinsics are `get` / `size` only (`try_resolve_arraylist_intrinsic`): no store, no barrier; its growth is the `copyOf` path above. Nothing to fix; the twelve-row probe below remains the verification. Also cross-checked: the new opt-in `CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL` (lane W8REV, intrinsic-coverage page) admits only primitive `fill` / `sort` and `CRC32C` sites as plain calls, so it adds no reference store shape.

> **j27 wave 5 (lane BARHUNT, 2026-10-28): PARTLY (no barrier defect found; one doc/code drift fixed, probe added), PENDING HOST VERIFICATION.** Reviewed the compiled reference-store barriers listed below and found no missing or wrong barrier I could prove from the code. Fixed the doc of `zgc_fresh_receiver_stores` (jit/src/ir_lower.rs), which said an allocation closes every fresh-receiver window; the scan lets a nested `Op::New` open its own window, and that is sound (argument below). Added the unit test `j27_barhunt_a_nested_new_keeps_the_outer_window_and_a_new_array_closes_it` (crate `cratonvm-jit`, module of `zs5bt2_fresh_receiver_stores_follow_the_window_rule`) and the probe `tools/probes/jit/j27/J27BarrierStressRows.java`. No switch. Verify: `cargo test -p cratonvm-jit -j 5 --lib j27_barhunt_a_nested_new`, then the probe under {ZGC, G1, Generational} x {--jdk-only, --compatible} x {default heap, -Xmx256m}, diffed against HotSpot 25 (expected lines below).

# Compiled GC barriers: j27 BARHUNT review

Status: review page (no open defect). Area: JIT reference-store barriers, both tiers, every collector.

## What was reviewed, and why each is sound

| Path | Collector | Finding |
|---|---|---|
| `putstatic` of a reference (both tiers, aarch64 too) | all | Always the `jit_putstatic_object` helper: `set_static_shared` does the SATB log; the value is pinned across the class-init guard. Statics are a side table scanned as roots, not a mirror card. Sound. |
| Fresh-receiver elision, Gen arm (`gen_fresh_store_no_pre_gate_for`) | Gen | Drops only the SATB pre gate (the old field is the allocation's null). The young-receiver post test stays, so an object that `try_alloc_object_old` returned old, or that a slow-path collection promoted, is still carded. Sound. |
| Fresh-receiver elision, ZGC arm (`zgc_fresh_receiver_stores`) | ZGC | No gate at all. SATB: the old value is null. Post: a promotion inside the window goes through `age_survivor` -> `card_object` (tenure-all included), and that card outlives the store. A nested `Op::New` keeps the window open (the doc said otherwise; fixed). `NewArray`, calls, loads and polls close it. Sound under that card rule. |
| G1 inline post filter (`emit_g1_post_write_barrier_regs`) and edge filter (`ir_g1_edge_filter`) | G1 | Subtracting the arena base keeps the same-region test exact. The edge filter only skips when the holder's card is dirty, its block offset is covered and the edge is recorded in this epoch. It has no StoreLoad fence between the store and the card read. That is fine only because G1 has **no concurrent refinement** here: a card is cleaned only inside a pause (`gc/src` has no concurrent card cleaner). **If concurrent refinement is ever added, this filter and the dirty-card skip in the Gen arms need HotSpot's StoreLoad membar** (`G1BarrierSetAssembler::g1_write_barrier_post`). |
| Gen constant-VarHandle card edge (`ir_lower/vh_gen_card_edge.rs`) | Gen | PRE + POST card checks around the access, and POST re-forms the card from the view's current words. The old->old skip is safe: an old object never becomes young, and a frozen peer's registers pin the holder. Sound. |
| `Object.clone` thin helper (`object_clone_serve_where`) and the heap's `copy_compact_object_fields` | all | Reference slots go through the barriered slot read and the collector's batch store, or through `get_field`/`set_field`. The copy is a fresh TLAB bump with no collection before it. Sound. |
| Deopt materialization of scalar-replaced objects (`deopt_materialize.rs`) | all | Shells are pinned across Phase-1 allocations, and Phase 2 uses `store_field_barriered`. That holds even when a Phase-1 collection promoted an earlier shell. Virtual arrays are primitive-only. Sound. |
| aarch64 `aastore` / `putfield` / `putstatic` of references | all | Helper calls (SATB + card inside). Sound. |

Not reviewed in depth (left to other lanes or unreachable without a build): the intrinsic arms of `System.arraycopy`, `Arrays.copyOf` and `ArrayList` growth in `ir_lower.rs` (INTRHUNT owns them), and the x64 single-pass post-only card completion under a BUG-03 take-over (covered by its own gcj2jt2 tests). The probe exercises all of them end to end.

## Probe

`tools/probes/jit/j27/J27BarrierStressRows.java`: twelve rows. Each row promotes its holders (two `System.gc()` calls), then for 24 rounds stores young `Box`es into the old holders from a compiled loop, churns young garbage (and runs `System.gc()` every 8th round), and only then reads every Box back. Each Box carries a check word.

Expected HotSpot 25 output (the same for every collector and heap size; computed in closed form, `a*50307072 + 565248` per row):

```
fact putfield sum=1560084480 bad=0
fact aastore sum=352714752 bad=0
fact putstatic sum=49404 bad=0
fact vh-cas sum=654557184 ok=49152 bad=0
fact vh-getandset sum=252100608 old=241572864 bad=0
fact vh-setrelease sum=553943040 bad=0
fact atomic-ref-array sum=1459470336 bad=0
fact arraycopy sum=151486464 bad=0
fact copyof sum=855785472 bad=0
fact clone sum=956399616 bad=0
fact arraylist sum=1157627904 bad=0
fact ctor-nested sum=101179392 bad=0
```

A row that differs, or a crash, is a lost old-to-young edge or SATB edge on that store shape. Rerun that row with the barrier switch of the shape set to `=0`: `CRATONVM_JIT_IR_FRESH_STORE_NO_BARRIER`, `CRATONVM_JIT_G1_EDGE_FILTER`, `CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE`, `CRATONVM_JIT_VH_CONST_GEN_CARD_INLINE` or `CRATONVM_JIT_POST_ONLY_CARD_MARK`. If the row flips, that switch names the culprit. Then file a defect page.

## What would close this page

The host battery shows the twelve lines SAME under all three collectors in both modes, and the unit test passes. The orchestrator then retires this page to `docs/internal/jit/`.
