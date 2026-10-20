# JIT round 21 wave 25: semantic review of the dev merge (lane review-merge)

Status: OPEN. 12 items. Each item is classified:

- **FIX-NOW:** small and clear, for a lane in this wave.
- **FILE:** stays on this page for the next round.

Item 7 is the only FIX-NOW defect. A JVMTI redefinition now installs the new `InnerClasses` entries with their raw
access flags, but since dev's zs s4/rp the define path stores them masked. It is fixed in this wave, pending the
orchestrator's build. Items 8 and 9 are FILE notes. Everything else is clean.

Scope: merge `bb5ca7d516` (dev `4c834913d8` into this branch; merge base `f79d328992`) and the fixups `edba4264b7`,
`6568926994`, `a560d04e14`, `c6e2138de4` and `8856941588`, read at HEAD `8856941588`.

How the review was done:

- The 37 conflicted files were recomputed with `git merge-tree --write-tree bb5ca7d516^1 bb5ca7d516^2`, which gives
  tree `c8f3428ad0`, the auto-merge with the conflict markers left in.
- `git diff c8f3428ad0 bb5ca7d516` shows every hand resolution. It touches only the 37 conflicted files and only
  their marker regions. So the merge made no extra edits: every other file is exactly git's auto-merge.
- Functions changed by both sides in non-conflicting hunks were found by intersecting the hunk headers of
  `git diff f79d328992 bb5ca7d516^1` and `git diff f79d328992 bb5ca7d516^2`, then read.

Nothing was built or run. Line numbers are HEAD's plus this lane's edit.

## Item status

| Item | Severity | Class | Verdict |
|---|---|---|---|
| 1 `runtime_lowering.rs`: `emit_alloc_prefetch` (ours) and `emit_zgc_alloc_prefetch` (dev) both kept after the bump's end check | Info | FILE (note) | clean: both opt-in, both write nothing; under ZGC with both switches set the lines are prefetched twice |
| 2 `x64.rs` `use objects::{..}` lists | Info | none | clean |
| 3 `op_array.rs` single-pass `aastore`: ours `g1_aastore`, dev's `fused_done`, ours `pre_gate` / `pre_skip` | Info | none | clean: no barrier skipped or doubled, the fused path is ZGC-only and never meets the G1 plan |
| 4 test-module resolutions in `ir_lower.rs` (`nio21w11_vh_const_tests`) and `classloader.rs` | Info | none | clean |
| 5 `InlineTlabPlan::post_init_helper_first` against dev's inline-TLAB changes | Info | none | clean: the field is OURS (r21 wave 2); dev's three new test literals rightly take `false` |
| 6 dev's `CpUtf8` / `CpLong` / `Box<EnclosingMethodInfo>` against our new code (fixups) | Info | none | clean |
| 7 our redefinition installs `InnerClasses` with raw flags; dev's define path masks them (`hotspot_stored_inner_classes`) | Low (reflection answers after a redefinition differ from HotSpot on bytes with unrecognised inner-class bits) | FIX-NOW | FIXED (this lane; pending orchestrator build), see the section |
| 8 our `HiddenClassBits::new` allocates eagerly; dev made its twins lazily zeroed (zs s6/rs) | Info (RSS) | FILE | OPEN |
| 9 dev's `ClassStore::footprint_census` does not count our two new indexes | Info (diagnostics) | FILE | OPEN |
| 10 `ARCHITECTURE.md` crate-size table kept dev's figures | Info (doc) | none | clean |
| 11 the other 30 conflicts: proposal-path citations, `vm_tlab.rs` test, flag docs, a `docs/internal/gc` page | Info | none | clean |
| 12 functions changed on both sides without a conflict | Info | none | clean |

## 1. Both allocation prefetches on one bump (Info, FILE note)

- **Where:** `jit/src/runtime_lowering.rs:1001-1003` (`new`) and `:2060-2062` (`newarray`). Our
  `emit_alloc_prefetch` (`:643`, `CRATONVM_JIT_IR_ALLOC_PREFETCH`, opt-in) is emitted first. Dev's
  `emit_zgc_alloc_prefetch` (`:1624`, `CRATONVM_JIT_ZGC_ALLOC_PREFETCH=<lines>`, opt-in, ZGC only through
  `announce != 0`) follows it.
- **Can both fire?** Yes, but only on ZGC with both switches set. The sequence is then ours (1 or 3 lines from
  `[RAX+192]`, `PREFETCHW` or `PREFETCHNTA`), then dev's (N lines from `[RAX+256]` by default, always `PREFETCHW`).
  The two overlap on part of the range.
- **Registers and flags:** neither emitter writes a register, a flag or memory. Both only read RAX, which is the new
  top at that point.
- **Ordering:** both run after `JA .slow`, so the cursor and end checks have passed. Both run before step 4/5's
  header stores, before any commit, and before the split / fall-through branch (`:1137`), so the split path carries
  them too.
- **Tests:** `r21w16_lean_*` (`:3246`, `:5148`) assert `on.len() == off.len() + lines`. They build non-ZGC plans
  (`announce = 0`), so dev's emitter adds nothing there.
- **Note for the A/B:** a run that sets both switches measures their sum. If either switch is ever flipped on, make
  the IR switch step aside where the ZGC plan already prefetches, or document that they stack.

## 2. `x64.rs` `use` lists (clean)

`jit/src/x64.rs:290-297`. Ours had `use objects::{ref_store_gates_of, ref_store_post_skip_mask_of}` plus
`{g1_aastore_plan_from, G1AastorePlan}`. Dev widened the first list with the five `zgc_*` items. The resolution keeps
dev's superset and our G1 line. Nothing is lost or duplicated.

## 3. `op_array.rs` single-pass `aastore` (clean)

`jit/src/x64/op_array.rs:672-866`. The order is now:

1. `aastore_gates`;
2. ours `g1_aastore` (`:686`), only when `aastore_gates.is_none()`;
3. dev's `fused_done` (`:701`), only when `aastore_gates` is `Some` and a fused word exists;
4. ours `pre_gate = aastore_gates.pre.or(g1.pre)` and `pre_skip` (`:716-719`);
5. the SATB load and call, the store, then the post arm;
6. `fused_done` patched last (`:864`).

Why this is right:

- **The fused path and the G1 plan never meet.** `fused_done` needs `aastore_gates`, and `g1_aastore` needs its
  absence. On ZGC's age-floor plan the G1 plan is `None`. On G1 (no published plan) there is no fused word.
- **Nothing is emitted twice.** Dev's side had its own inline `pre_skip` (`MOV R11, pre; CMP [R11], 0; JE`). The
  resolution replaced it with ours, `emit_sp_aastore_satb_gate_skip` (`objects.rs:3512`), which emits the same three
  instructions. There is one SATB gate per site.
- **Nothing is skipped.** The fused `JE` path (both gate bytes zero) stores and jumps to after the post arm. That is
  exactly where the full sequence's barrier-free arms land: pre clear, then null / young / post clear. On any other
  word the `JNE` path falls into the unchanged sequence, with RAX = the array and RCX = the index. The fused compare
  touches only R11 and the flags, and `pre_skip` reloads R11 itself.
- **Inline card mark:** the fused path is still excluded (`!self.inline_card_mark_available()`) and needs the
  floor-shaped plan, so the generational mask plan never reaches it.

## 4. Test-module resolutions (clean)

- `jit/src/ir_lower.rs`: ours `r21w16_cf_a_null_old_value_store_stays_inline_while_marking_only_under_the_switch`
  (`:76923`) and dev's `zs5bt2_the_fused_screen_stores_exactly_where_the_full_screen_does` (`:77036`) are now
  sibling functions of `nio21w11_vh_const_tests`. The `}` the resolution adds closes ours.
- Our test uses a MASK plan (`mask: Some(0x10)`, `floor: 0`), so dev's fused prefix (`vh_fused_gate_word` needs no
  mask and a non-zero floor) adds no bytes to its screen. Its byte-length comparison still holds.
- `native-builtins/src/classloader.rs:24668-25760`: ours `mod r21_lookup_in_relation_tests` is closed by the added
  `}\n}`, and dev's modules (`zt t2/nd` … `zt7cl_add_class_tests`) follow it whole.

## 5. `InlineTlabPlan::post_init_helper_first` (clean)

- **Whose field it is:** the brief calls the field dev's, but it is OURS. `git grep` finds it on `bb5ca7d516^1` and
  not on `^2`. It was added in JIT r21 wave 2, lane alloc, `CRATONVM_JIT_IR_ZGC_POST_INIT_HELPER_FIRST`.
- **Fixup `8856941588`:** it gave dev's three new test literals `false`. They are `zs6ca3_align_test_tests::new_plan`
  (`skip_post_init: true`, so the field is irrelevant), `zs9ca4_fall_through_tests::new_plan` (`announce != 0`, so
  `helper_first` is already true through the announce term at `:1053`) and `zs1ca_inline_arm_fast_join_tests::new_plan`.
  In the third, dev's tests were written for the pre-r21 commit-then-call order, so `false` is that side's intent.
- **Production sites:**
  - `ir_lower.rs:34768` keeps the proven expression: registration required, the class finalizer-free, and the switch.
  - The two box plans (`:19265`, `:19540`) are `skip_post_init: true`.
- **Dev's split path:** `emit_inline_tlab_new_ir_split` admits only `announce != 0` (`:884-892`), where the field
  does not matter. Dev's shared helper arm `emit_new_announce_helper_arm` (`:1208`) takes `helper_first` from
  `emit_inline_tlab_new_ir_impl`, so our ordering survives on both the unsplit path and the cold half.
- **Our any-GC `anewarray`:** dev's new newarray paths (`zgc_newarray_skip_word_round`, the size-off-chain
  footprint, the constant-length path) are all gated on `announce != 0`, so they never reach our non-ZGC
  `anewarray` (`ir_inline_anewarray_any_gc_enabled`).

## 6. `CpUtf8`, `CpLong`, boxed `EnclosingMethodInfo` (clean)

- `classloading/src/bytecode_verifier.rs:170`: `Utf8((&class.name).into())` goes through `From<&Arc<str>> for CpUtf8`.
  The text is the same and the wide side table is untouched.
- `class_manager.rs`:
  - `redefinition_inner_tables` unboxes `nesting.enclosing_method.map(|b| *b)`;
  - the install re-boxes it (`.map(Box::new)`);
  - the define path assigns the `Box` directly.
- `jit-cuda` `ldc2_w` reads `v.get()`. The test pools in `reader/src/attribute.rs` and `exception_dispatch.rs` use
  `.into()`.
- No `Entry::Utf8(Arc::from|clone ..)` remains anywhere: `rg 'Entry::Utf8\((std::sync::)?Arc::'` finds only
  `instrument_reconstitute.rs:804`, which already calls `.into()`.
- Our new `get_utf8(..) == Some(name)` comparisons still compare `Option<&str>`.

## 7. Redefinition installs raw `InnerClasses` flags (Low, FIX-NOW: FIXED)

- **What dev changed:** zs s4/rp made the define path store each `InnerClasses` entry's `inner_class_access_flags`
  as HotSpot's `ClassFileParser` does: masked to `RECOGNIZED_INNER_CLASS_MODIFIERS`, plus `ACC_MODULE` from version
  53, plus `ACC_ABSTRACT` for a pre-50 interface. This is `hotspot_stored_inner_classes`
  (`classloading/src/class_manager.rs:873`), called at `:11873` and `:18491`.
- **What ours does:** r21 wave 17 (lane innerclasses, `CRATONVM_REDEFINE_SWAPS_INNER_CLASSES`, default ON) made
  `redefine_class` install the new bytes' table through `redefinition_inner_tables` (`:14787`). That table was
  `NestingAttributes::of(..).inner_classes` RAW.
- **Effect:** after a redefinition, any reflective answer that reads the stored entry flags could include bits
  HotSpot drops. Examples are `Class.getModifiers` of a member class through its `InnerClasses` row,
  `JVM_GetDeclaredClasses` / `getSimpleBinaryName` consumers, and JVMTI class modifiers. Before the redefinition the
  same class answered masked. HotSpot's scratch class comes from the same `ClassFileParser`, so it answers masked
  both before and after.
- **Fix (this lane):**
  - `redefinition_inner_tables` takes the new bytes' `major` and returns
    `hotspot_stored_inner_classes(inner_classes, major)` (`:14831`).
  - Both callers pass `new_class_file.version.major`: `check_redefinition` (`:14868`) and `redefine_class_typed`
    (`:15586`, read after the `methods` / `constant_pool` / `attributes` partial moves, which is legal: `ClassFile`
    has no `Drop`).
  - No switch: this restores dev's intent on our path, and `CRATONVM_REDEFINE_SWAPS_INNER_CLASSES=0` still keeps the
    old table.
- **Test to add:** extend `a_redefinition_installs_the_new_inner_classes_and_enclosing_method` so the new bytes'
  entry carries an unrecognised bit (for example `0x0080` on a `0x0019` member), and assert that the installed entry
  reads `0x0019`.

## 8. `HiddenClassBits::new` allocates eagerly (Info, FILE)

- **Where:** `class_manager.rs:4437-4446`. Our hidden-class bitmap (r21 waves 14/22) builds `words` and `self_names`
  with `(0..N).map(|_| AtomicU64::new(0)).collect()`. Its doc says it has the "same layout and capacity as
  `JavaUtilClassBits`".
- **What dev changed:** dev's zs s6/rs moved `JavaUtilClassBits` and `ClassInitBits` to
  `cratonvm_types::zeroed_table::zeroed_arc_slice`, so their pages stay untouched (`CRATONVM_LAZY_ZERO_TABLES`).
- **Effect:** ours still touches every page at `ClassManager` construction. It is RSS, not behaviour.
- **Fix:** use the same `zeroed_arc_slice` for both arrays.

## 9. `ClassStore::footprint_census` misses our indexes (Info, FILE)

- **Where:** dev's census (`classloading/src/class.rs:3339`) lists the store's own index tables. It does not count
  our `finalizable_ancestors` (`FxHashSet<u32>`) or `recorded_file_names` (`FxHashMap<u32, Arc<str>>`), both added
  in r21.
- **Effect:** the count is low by those tables, which are empty in most VMs. It is diagnostics only.
- **Fix:** add two rows.

## 10. `ARCHITECTURE.md` crate-size table (clean, doc)

The resolution kept dev's re-measured figures (`vm` 1,027k, `jit` 692k, `gc` 416k) over ours (`jit` 718k), under
dev's dated heading. The table is a dated measurement, so this is consistent. The next re-measure picks up this
branch's `jit` growth.

## 11. The other conflicts (clean)

- **Proposal-path citations (28 files):** every one took dev's full `docs/internal/proposals/interpreter/...` path.
- **`gc/src/zgc/vm_tlab.rs` `infallible_retirement_tls_tests`:** the resolution took dev's record scan of
  `OWNED_VM_TLAB_CHUNKS`. Our side had swapped in `owned_vm_tlab_chunk_raw` instead of the epoch read. Both fix the
  same cross-test epoch race, and the helper is still used elsewhere.
- **`docs/config/flag-inventory.md`, `docs/flag-tokens.md`:** regenerated. The counts were not re-verified here.
- **`docs/internal/gc/zgc-tlab-refill-ignores-the-object-floor-FIXED-20261019.md`:** the rename was kept with dev's
  FIXED status line.

## 12. Functions changed on both sides without a conflict (clean)

- **`vm/src/jit/helpers.rs`:**
  - `jit_newarray_body`: ours adds the engagement census, dev adds the ZGC rate limit and the stamped failure door.
    These are disjoint statements.
  - `jit_indy_bridge_body`: ours adds the bimorphic guarded leaf at entry, and dev only edits a doc comment.
- **`jit/src/lib.rs` `try_compile_inner` / `ir_tier_attempt`:** dev's `CRATONVM_DBG_JIT_SINGLE_PASS_ONLY` refusal is
  one more conjunct and one more splice `continue`. It is debug-only.
- **`jit/src/ir_lower.rs` `vh_ref_store_gate_screen_with`:** dev's fused `JE` (both gate bytes zero) is put in front
  of our null-old SATB screen. The fused edge is sufficient on its own: pre clear means no old value to log. Any
  other word runs our screen unchanged.
- **Dev's `CRATONVM_JIT_ZGC_FRESH_RECEIVER_STORE` (opt-in):** it needs a same-block `Op::New` with no call in
  between, so no safepoint or JVMTI `GetLocalObject` can intervene. Our wave-24 JVMTI-locals gate for the
  fresh-volatile-store elision is not needed there.
- **`vm/src/runtime/interpreter/jit_bridge.rs` `CpResolvers`:** ours adds `second`, `mega_site_bind` and
  `jvmti_locals_accessible`, and dev changes `CpLong` reads. These are disjoint.
- **`native-builtins/src/unsafe_natives.rs` `native_unsafe_define_anonymous_class`:** ours makes the prefix the class
  file's own name, and dev adds the mirror reservation around the define. Both are kept, in order.
- **`ClassStore::add`:** dev publishes `next_id_cell` after the push. Our branch adds no other push site.
- **`lang_class.rs`:** dev's `listing_component_id` covers the declared-member listings. The remaining
  `reflection_component_id` callers (`collect_public_*`, `native_class_get_constructors`) predate both sides.
