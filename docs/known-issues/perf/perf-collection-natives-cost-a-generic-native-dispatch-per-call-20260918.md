# `ArrayList.get` / `HashMap.get` / `HashMap.put` from compiled code are a generic native dispatch each

**Status:** PARTIALLY FIXED (round 9 wave 3, collections3; wave 4, vm4; wave 8, arraylist8) -- native side (proposed fix 2) and the VM-side leaf helper are done; proposed fix 1 (the exact-`ArrayList` `get`/`size` JIT intrinsic) is landed in the SINGLE-PASS tier at every compile door (wave 8, see "Status after wave 8"). Wave 9 (irl9) landed the optimizing-tier (IR) lowering of the same fast path in `ir_lower.rs`; it is armed once `lib.rs` passes the layout (`NOTES-w9-irl9.md` cross-lane request 2; see "Status after wave 9"). Wave 9 (evict9): that call-site edit is APPLIED in `lib.rs`, and a single-pass PIN for methods with a prefix site landed (the measured C2 downgrade, see "Status after wave 9 (evict9)"); which of the two bodies wins is the integrator's A/B. Remaining: that A/B, and the `Integer` boxing allocation (fix 3). Wave 10 (collect10): the IR-tier box/unbox fold landed (`valueOf` read back only by `intValue`/`longValue` is deleted), and the measured split of the boxing cost plus two exact cross-lane edits (cached-range `valueOf` without the `safe_native_call` funnel; Fx maps for the per-VM cache tables) are filed -- see "Status after wave 10 (collect10)"; the escaping-box allocation itself stays open. Wave 10b (wire10b): both cross-lane edits LANDED (plus the `Long` twin; `INTEGER_CACHE_HIGH` Fx-hashed, `ScopedIntegerCache` left SipHash) -- see "Status after wave 10b (wire10b)". Wave 11 (collect11): the `IntegerCache` read is lock-free and hash-free (atomic per-VM table + last-table pointer) for the JIT helper, the peek and the native -- see "Status after wave 11 (collect11)"; the escaping-box allocation (JIT inline `valueOf` allocation) and the `HashMap` native side stay open.
**Owner-area:** `../../../native-collections/src/lib.rs` (`native_al_get` registered on
`java/util/List`/`ArrayList` `get(I)`, `native_hashmap_put_exact`, `try_hm_int_fast_put`,
`al_slots_and_layout_for`, `class_facts`, `class_name_rc`), `../../../vm/src/jit/helpers.rs`
(`jit_invoke_virtual_mic` → `try_jit_site_cached_native_dispatch` →
`vm_exec::safe_native_call_impl`, `jit_hashmap_get_direct`), `../../../vm/src/vm/vm_exec.rs`
(`hashmap_string_node_cache_get_object`, `deadrecv_check`,
`resolve_field_descriptor_byte_cached`), `../../../jit/src/lib.rs` (native-shadow inline refusal).
**Found by:** JIT review round 9, wave 2, lane `perfdiag`, 2026-09-18.
**Related:** `object-hashcode-on-a-string-receiver-pays-the-mic-native-helper-20260918.md`
(same funnel, different method); `../../internal/performance/hashmap-half-gap-20260730.md`.

## Evidence

Three of the measured gaps spend most of their CPU inside Rust collection natives reached
from compiled code, with JIT code itself under 10 %:

**`SpliceCastProbe`** (1 147 ms vs 6 ms; 4 M steps = ~287 ns per `step`), sampler, 2 219
busy samples:

```text
16.3%  zgc::ZgcRealHeap::is_object_address      3.2%  native_collections::al_slots_and_layout_for
 8.2%  vm_exec::safe_native_call_impl           3.0%  VmHeap::load_and_forward_inner
 6.2%  try_jit_site_cached_native_dispatch      2.6%  zgc::get_array_element
 4.1%  jit_invoke_virtual_mic_body              2.4%  resolve_field_descriptor_byte_cached
 4.1%  VmHeap::is_object_address               2.4%  decode_dispatch_values_into_shaped
 3.7%  vm_exec::deadrecv_check                 1.5%  native_collections::class_name_rc
 4.5%  jit: SpliceCastProbe.step [c2]          1.1%  native_collections::native_al_get
```

**`CratonBenchC2 pipeline`** (346–362 ms/round steady state vs 5 ms): the same frames in
the same order (`is_object_address` 15 %, `safe_native_call_impl` 8 %,
`try_jit_site_cached_native_dispatch` 5 %, `class_facts`, `al_slots_and_layout_for`,
`deadrecv_check` ...) — `runBatch`'s `batch.get(i)`.

**`CratonBench hashmap`** (8.2 s vs 0.63 s): `jit_integer_value_of_direct` 7.4 %,
`vm_exec::alloc_object` 5 %, `safe_native_call_impl` 4.2 %, `try_hm_int_fast_put` 3.9 %,
`VersionCache` layout lookups 3.8 %, `set_field_no_card` 3 %, `get_field` 2.8 %,
`is_object_address` 2.6 %+1 %, `native_hashmap_put_exact` ... and 1.2 % in the OSR'd
loop itself. **`CratonBenchC2 bind`**: `hashmap_string_node_cache_get_object` 7.1 %,
`jit_hashmap_get_direct` 2.3 %, plus the field accessors.

## Root cause

`ArrayList.get`, `HashMap.get/put` (and friends) are native-shadowed: the Rust
implementation replaces the JDK bytecode. That has two costs in compiled code:

1. **No inlining.** The inliner refuses native-shadowed callees, so a three-instruction
   `elementData[index]` read (after `checkIndex`) is a CALL into the MIC helper, then
   the site-cached native dispatch (argument decode, `deadrecv_check`), then
   `native_al_get` itself, which re-derives the receiver's layout by class name
   (`class_facts`, `class_name_rc`, `al_slots_and_layout_for`) and reads fields and the
   element array through the checked heap API — each access doing an
   `is_object_address` membership walk (16–20 % of CPU on its own).
2. **Boxing/allocation through the slow path.** `HashMap<Integer,Integer>.put` goes
   through `jit_integer_value_of_direct` → `vm_exec::alloc_object`, i.e. the helper
   allocation path of `perf-compact-tlab-planner-reads-an-env-var-per-allocation-20260918.md`.

## Proposed fix

In order of cost/benefit:

1. **`ArrayList.get(int)` JIT intrinsic** for a receiver whose class id is exactly
   `java/util/ArrayList` (guard on class id, which the MIC already knows): bounds check
   against `size`, load `elementData`, `aaload`. Same for `size()`. This is the shape of
   every `for (int i...; list.get(i))` loop and the whole of SpliceCastProbe/pipeline.
   Alternatively, stop shadowing `ArrayList.get`/`size` for exact-`ArrayList` receivers
   so the real bytecode (which the IR tier can splice) runs.
2. Inside the natives, validate the receiver ONCE per call (the getfield page's "ZGC,
   part 2" pattern: `is_object_address` once, then unchecked slot reads), and cache the
   `(class id → layout)` answer instead of resolving by name per call.
3. For `HashMap<Integer,…>`, allocate the `Integer` box through the inline TLAB (see the
   ZGC page) or keep the int-key overlay unboxed end-to-end.

## Expected gain

(1) removes essentially all of SpliceCastProbe's and pipeline's excess (10–50x on those
microbenchmarks; any list-indexing loop in real code). (2) is a 20–30 % cut of every
remaining collection native call. hashmap: (2)+(3) plus the allocation pages, ~30–40 %.

## How to verify

```bash
cd C:/craton/jitr9-probes
python diag/sampler.py 0.6 3 -- "$VM" --java-home "$JH" -cp cls SpliceCastProbe 20000000
"$VM" --java-home "$JH" -cp "cls;diag/cls" C2Loop pipeline 4
# after (1): no safe_native_call_impl / native_al_get / is_object_address in the top 10
```

## Status after wave 3

### Done (native-collections side, lane collections3)

All in `../../../native-collections/src/lib.rs`; no registration, `NativeKind` or
`stub_ratchet` change -- the registered natives are the same, they are cheaper.

- **`al_fast_state`** (next to `al_slots_and_layout_for`): one hot-path read of
  an ordinary `java.util.ArrayList`'s `(elementData, size, length)`. One
  `class_id_of_object`, the existing per-class `AL_SLOTS` memo (now reachable by
  `ClassId` through the new `al_slots_and_layout_for_class`), the `class_facts`
  memo to exclude every wrapper/view route (`CF_UNMOD_WRAPPER`,
  `CF_KEY_SET_VIEW`, `CF_VALUES_VIEW`, `CF_MAP_ENTRY_VIEW`), one header read,
  ONE `get_fields_typed` for both slots (one forwarding barrier, no
  `resolve_field_descriptor_byte_cached`), ONE `array_shape` (kind + length
  under one membership walk), and the `view_source_in` marker test. Returns
  `None` for anything the general path special-cases, which then runs unchanged.
- **Fast paths** at the top of `native_al_get`, `native_al_size`,
  `native_al_is_empty`, `native_al_set`, and `native_al_add` (spare capacity
  only; a grow still takes the pinned general path). Before, one
  `ArrayList.get` on a plain list made five `class_id_of_object` reads, a
  `class_name_rc` + six string compares (`singleton_wrapper_size`), two
  descriptor-resolving `get_field`s, `heap_kind_of`, `array_length` and the
  element read -- each a checked heap access. Now: class id, header, one batched
  field read, one `array_shape`, (one trailing-slot read when `size < length`),
  the element.
- **`is_immutable_jdk_stand_in`** (every `ArrayList.add`) used
  `class_name_of_id` -- class-manager read lock plus a fresh `String` per call;
  now the per-thread `class_name_rc` memo.
- **`abstract_list_mod_count_slot`**: `al_mod_count_slot` (every
  `add`/`remove` via `al_set_size`, and the list iterator's per-element
  comodification check) ran a string-keyed
  `resolve_field_index("java/util/AbstractList", "modCount")` each call; now
  memoized per thread and VM (resolved answers only).
- **`jit_arraylist_get` / `jit_arraylist_size`** (pub, next to
  `native_al_get`): no-allocation / no-Java / no-throw probes with the same
  contract as `jit_overlay_hashmap_get`, for a VM-side thin helper to call
  without `safe_native_call` (see below).

Tests (`../../../native-collections/src/lib.rs`, `mod tests`):
`al_fast_state_answers_a_plain_arraylist_like_al_state`,
`al_fast_add_on_a_full_backing_array_still_grows`,
`al_fast_state_declines_every_shape_the_general_path_special_cases`,
`abstract_list_mod_count_slot_memo_matches_the_resolver_per_vm`.

### Remaining

1. **The VM dispatch funnel** (`jit_invoke_virtual_mic` ->
   `try_jit_site_cached_native_dispatch` -> `safe_native_call_impl`,
   `deadrecv_check`, `decode_dispatch_values_into_shaped`) is ~25 % of
   SpliceCastProbe and is NOT in this lane's files. Filed as a cross-lane
   request to vmhelpers3 in `../../internal/jit-review-r9/NOTES-w3-collections3.md`:
   serve a site-cached `native_al_get`/`native_al_size` hit through
   `jit_arraylist_get`/`jit_arraylist_size` without the `safe_native_call`
   wrapper, as `jit_hashmap_get_direct` already does for the HashMap overlay.
2. **Inlining / JIT intrinsic** for exact-`ArrayList.get`/`size` (proposed fix 1)
   -- JIT-side, untouched.
3. **`CratonBench hashmap`**: the collection natives are ~6 % of it
   (`try_hm_int_fast_put` 4 %, `DenseIntEntries::insert` 1.7 %); the rest is the
   `Integer.valueOf` allocation path (`plan_tlab_object_shape_at` 18.9 %,
   `jit_integer_value_of_direct` 7.9 %, `alloc_tlab` 6.2 %, `alloc_object` 5.2 %,
   `VersionCache` layout lookups 4.3 %, env-var flag reads) -- the allocation
   pages, not this crate.

Baseline on the wave-2 binary (2026-09-18, this box): SpliceCastProbe 1252 ms,
CratonBenchC2 pipeline 370 ms, CratonBench hashmap 9466 ms. The wave-2 binary
predates these edits, so the after-numbers are for the integrator's build.

## Status after wave 4 (`vm4`, VM side of proposed fix 1)

The collections3 cross-lane request landed in `../../../vm/src/jit/helpers.rs`:

* `resolve_native_site` classifies a resolved site once, in `collection_leaf_kind`, into
  the new `LeafNativeKind::ArrayListGet` / `ArrayListSize`. The key is the ADMITTED
  callback's identity (`native_al_get` / `native_al_size`, compared as addresses) plus the
  call-site descriptor. Only virtual and interface sites qualify.
* `try_jit_site_cached_native_dispatch` serves those hits with
  `cratonvm_native_collections::jit_arraylist_get` / `jit_arraylist_size`. It skips the
  argument decode and the `safe_native_call_*` wrapper, after the receiver guard, the
  redefinition checks and the capability gate. A `None` from the probe falls through to the
  entry's ordinary callback path, which runs the same native. An object return roots the
  element through `thread.native_pending_return`.
* JDK-only: under `--jdk-only` the probe stands aside, like `jit_direct_helper_refused`,
  and the admitted callback runs. This is not counted as a direct-helper refusal.
* Counter: `jit_collection_leaf_hit_count()`. Printing it under `intrinsic-stats` is a
  cross-lane request to `vm-cli`.
* Test: `r9w4_vm4_collection_leaf_kind_tests::arraylist_sites_classify_by_callback_identity`.

`HashMap.get` already had its own direct helper. The `Integer` boxing (fix 3) is still
open, so the page stays PARTIALLY FIXED.

## Status after wave 5 (`collect5`, proposed fix 1 -- design and contract, not landed)

### Measured before (wave-4 binary, this box, 2026-09-18)

| probe | CratonVM | HotSpot |
|---|---:|---:|
| `SpliceCastProbe` (4 M steps, default tiers) | 614 / 635 ms | ~6 ms |
| `SpliceCastProbe`, `CRATONVM_C2_SUPERSEDE=0` (single-pass body) | 741 ms | |
| `CratonBenchC2 pipeline` (40K x 16) | 192 ms | ~5 ms |

Sampler (`diag/sampler.py 0.8 3`, 30 M steps, default tiers), 1 932 busy samples:
`ZgcRealHeap::is_object_address` 13.5 %, `try_jit_site_cached_native_dispatch` 11.2 %,
`SpliceCastProbe.step [c2]` 7.9 %, `jit_invoke_virtual_mic_body` 5.0 %,
`VmHeap::is_object_address` 4.9 %, zgc `get_array_element` 4.5 %,
`coerce_field_value_for_slot` 4.0 %, `al_fast_state` 4.0 %, `load_and_forward_inner` 4.0 %,
`read_prim_element` 3.2 %, ... `try_jit_arraylist_leaf` 1.6 %. The wave-4 leaf path is live
(`try_jit_arraylist_leaf` is on the stack) and removed the `safe_native_call_impl` /
`decode_dispatch_values_into_shaped` frames. What remains is the MIC helper round trip plus
`al_fast_state`'s checked heap accesses: class id, `object_num_fields`, `get_fields_typed`,
`array_shape`, and -- because a 16-element list grown from the default capacity has
`elementData.length == 22 > size` -- the `view_source_in` marker read, then the element.
The step's own compiled code is 8 %.

### Why it is not landed from `op_invoke.rs` / `intrinsic_catalogue.rs`

1. **The measured call sites run in the OPTIMIZING tier.** `SpliceCastProbe.step` and
   `CratonBenchC2.runBatch` are `[c2]` bodies; their `List.get` is lowered by
   `ir_lower.rs`, not by `x64/op_invoke.rs`. The IR tier has no node that reads an
   `ObjectHeader` class id (`ir.rs`, the String-intrinsic expansion: it refuses every
   class-id-guarded String site for exactly that reason), so a single-pass intrinsic would
   move neither benchmark. Only with `CRATONVM_C2_SUPERSEDE=0` does a single-pass `step`
   run (741 ms).
2. **The emitter has no way to learn the layout.** An exact-class intrinsic needs, at
   compile time, `java/util/ArrayList`'s class id and the abstract field indices of
   `elementData` and `size` -- per VM, since class ids are per VM and `../../../AGENTS.md` forbids a
   process global. The single-pass `Compiler` receives no name-to-id or field resolver.
   The precedent (`StringBuilderFieldLayout`) arrives on `StringFieldLayout`, which is
   built by `vm/src/runtime/interpreter/dispatch_static.rs::resolve_string_field_layout`
   and defined in `../../../jit/src/lib.rs` -- neither owned by this lane.
3. **Registration lives in `lib.rs`.** A site becomes an intrinsic only when
   `try_compile_inner`'s scan (and the OSR door in `vm/.../jit_bridge.rs`) pushes a
   `JitDirectCall` carrying the sentinel, and a sentinel that DECLINES to a call also needs
   a `JitInvokeInfo` minted at both doors (`string_intrinsic_declines_to_a_call`). A site
   registered at one door and not the other drops the whole method to the interpreter
   (measured on the StringBuilder family: 404 696 interpreted dispatches against 8 695).
   Landing only the emitter half would be dead code; landing it reading a `lib.rs` field
   that does not exist yet would break the build.
4. **How the native wins today.** `vm_exec::resolve_dispatch` says real bytecode beats a
   `Bridge` (`native_al_get` is registered as `Bridge` on `java/util/ArrayList.get`,
   `java/util/List.get`, and `native_al_size` on `ArrayList.size`, `List.size`,
   `Collection.size`), but compiled code never asks it: `jit_invoke_virtual_mic` ->
   `resolve_native_site` resolves by the RECEIVER's class name straight in the native
   registry (`resolve_native_owner_for_receiver`), so for an exact `java/util/ArrayList`
   receiver the answer in `Compatible` mode is `native_al_get` / `native_al_size`. That is
   item 1 of the JDK-ONLY-NOTE in `../../../jit/src/lib.rs`. Under `--jdk-only` these rows are
   `Bridge`, not `Intrinsic`, so an intrinsic standing in front of the real bytecode must
   be REFUSED there, exactly like the HashMap thin helpers (`direct_native_helper_for_impl`).

### The design (inline, exact class, both tiers)

Semantics: for a receiver whose header class id is EXACTLY `java/util/ArrayList`, answer
precisely what `native_al_get`'s wave-3 fast path answers (`al_fast_state` plus an in-range
index), and take the UNCHANGED dispatch -- the MIC/PIC call the site has today -- on every
other outcome: null receiver, another class (subclasses, `Vector`, views,
`ImmutableCollections`), a null or non-reference-array `elementData`, the values-view
marker, an index outside `[0, size)` or outside `[0, elementData.length)`. The dispatch
then runs the registered native, which throws the plain `IndexOutOfBoundsException` or the
NPE exactly as today. Never an uncommon trap: an out-of-range probe is ordinary control
flow in real code (`catch (IndexOutOfBoundsException)` loops), and a deopt would re-run the
method in the interpreter every time.

Single-pass lowering (RAX = receiver, R9 = index; `decline` = the unchanged dispatch):

```text
  [layout epoch guard if jit_sp_field_layout_guard_enabled()]  ; JNE decline (clobbers RCX/R11)
  TEST RAX,RAX ; JZ decline
  CMP  BYTE [RAX+KIND_TAGS_BYTE_OFFSET],0 ; JNE decline   ; plain object: an array carries
                                                           ; its COMPONENT's id at +0
  CMP  DWORD [RAX+0],al_class_id ; JNE decline
  TEST BYTE [RAX+GC_FLAGS_BYTE_OFFSET],GC_FLAG_COMPACT ; JZ legacy
  compact: MOV ECX,[RAX+size_c] ; MOV R8,[RAX+data_c] ; JMP have
  legacy:  CMP DWORD [RAX+size_cell+TAG],0 (Int) ; JNE decline
           CMP DWORD [RAX+data_cell+TAG],FIELD_CELL_TAG_OBJECT ; JNE decline
           MOV ECX,[RAX+size_l] ; MOV R8,[RAX+data_l]
  have:
  TEST R8,R8 ; JZ decline                                    ; elementData null
  CMP  BYTE [R8+KIND_TAGS_BYTE_OFFSET],0x01 ; JNE decline    ; kind=Array, elem=Reference
  MOV  EDX,[R8+ARRAY_LENGTH_OFFSET]
  get: CMP R9D,ECX ; JAE decline                             ; unsigned: catches negative
       CMP R9D,EDX ; JAE decline                             ; never an unchecked read
  CMP  ECX,EDX ; JAE nomark                                  ; view_source_in, verbatim:
  CMP  QWORD [R8+RDX*8+ARRAY_DATA_OFFSET-8],0 ; JNE decline  ; trailing non-null slot
nomark:
  get:  MOV RAX,[R8+R9*8+ARRAY_DATA_OFFSET]  -> push, mark_top_as_oop
  size: MOVSXD RAX,ECX                        -> push
  JMP done
decline: <the site's existing MIC/PIC dispatch, unchanged>
done:
```

`size()` omits the two index compares and keeps every other check (`al_fast_state`'s `None`
cases are exactly what `native_al_size` special-cases). Refused at LAYOUT construction, so
registration and emission cannot disagree: narrow oops (a 4-byte compact `elementData`, as
`StringBuilderFieldLayout::new` refuses its `value`), a compact `size` storage width other
than 4, class id 0, `zgc_read_barrier_blocks_inline_fields()` at compile time,
`CRATONVM_DBG_ALTRACE` set (the native's trace must still see every call), and a kill
switch `CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS=1` (the family's A/B lever, the
`CRATONVM_NO_JIT_SB_INTRINSICS` shape; needs an INVENTORY row). The reference loads are
raw, exactly as the String / StringBuilder `value` loads are, and sound on the same terms
(`zgc_codegen_honours_read_barrier`); the compile-time ZGC refusal is the one the inline
getfield takes.

Optimizing tier: an `Op::ArrayListAccess { kind, recv, index }` lowered in `ir_lower.rs` to
the same sequence with an ordinary `Op::Call` on the decline edge -- or, more generally, the
`GuardClassId` node `NOTES-invoke.md` proposal 7 asks for. Until one exists the IR builder
must treat the sentinel exactly like a StringBuilder decline site (an ordinary call through
the minted `JitInvokeInfo`), so an optimizing body is never worse than today.

### Cross-lane contract (exact names)

* `../../../jit/src/intrinsic_catalogue.rs` (collect lane): a new region `ARRAYLIST_ACCESS`
  appended after `FFM_SEGMENT` with `ArrayListGet` (`get(I)Ljava/lang/Object;`, 1 param,
  ret `b'L'`) and `ArrayListSize` (`size()I`, 0 params, ret `b'I'`); `LAST` moves to
  `ArrayListSize`. Beside it
  `pub struct ArrayListFieldLayout { pub size_compact_offset: i32, pub size_legacy_offset: i32,
  pub data_compact_offset: i32, pub data_legacy_offset: i32, pub class_id: u32 }` with
  `pub fn new(data_field_index: usize, size_field_index: usize, class_id: u32) -> Option<Self>`
  (the refusals above), and
  `pub fn try_resolve_arraylist_intrinsic(class: &str, name: &str, descriptor: &str,
  layout: Option<ArrayListFieldLayout>) -> Option<(usize, usize, u8, u32)>` answering for
  site classes `java/util/ArrayList`, `java/util/List` (`get`, `size`),
  `java/util/Collection` (`size`), guard = `layout.class_id` (never 0).
* `../../../jit/src/lib.rs`: `StringFieldLayout` gains `pub array_list: Option<ArrayListFieldLayout>`
  (`None` in `StringFieldLayout::new`) and `pub fn with_array_list(self, ..) -> Self`, the
  `with_builder` twin. `try_compile_inner`'s scan registers the site when `!jdk_only` and
  `try_resolve_arraylist_intrinsic(..)` answers, with BOTH the site row and
  `java/util/ArrayList.<member>` admitted by `direct_native_helper_for_impl`'s policy
  (i.e. refused under `--jdk-only`). `string_intrinsic_declines_to_a_call` (or a sibling
  `arraylist_intrinsic_declines_to_a_call`, asked by both doors) returns `true` for both
  entries, so both doors mint the `JitInvokeInfo` and the MIC/PIC slots the decline edge
  needs.
* `vm/src/runtime/interpreter/dispatch_static.rs::resolve_string_field_layout`: under the
  same read lock, `find_bootstrap_class_by_name("java/util/ArrayList")`, then
  `find_field_recursive_by_descriptor(al_id, "elementData", "[Ljava/lang/Object;", store)`
  and `(al_id, "size", "I", store)` -- the same indices `native-collections`'
  `al_slots_resolved` gets from `resolve_field_index` -- then
  `.with_array_list(ArrayListFieldLayout::new(data, size, al_id.as_u32()))`. The OSR door
  passes the same layout (it already passes `StringFieldLayout`).
* `../../../jit/src/x64/op_invoke.rs` (collect lane): the region above, in the `intrinsic_handled`
  ladder. Decline edge = the site's normal virtual/interface dispatch tail (MIC/PIC), NOT
  `invoke_dispatch` with a static info, so the native-site cache hit it has today is kept.
  An `ARRAYLIST_INTRINSIC_SITES` counter.
* `../../../jit/src/ir.rs` / `ir_lower.rs` (IR lane): the optimizing-tier lowering, or the
  pass-through.

Tests the landing must carry (executed, `../../../jit/tests`): a hit (`get(i)` on a hand-built
compact ArrayList-shaped object and on a legacy one), `size()`, a class-id miss (another
id -> the stubbed dispatch helper runs), out-of-bounds (`-1`, `size`, `size <= i < length`
-> dispatch; the IOOBE itself is the native's and is covered in `native-collections`), a
null receiver (-> dispatch, which reports the NPE), the values-view marker (-> dispatch),
and a primitive `elementData` (-> dispatch).

### A cheaper intermediate step that serves BOTH tiers (also cross-lane)

A thin direct helper `jit_arraylist_get_direct(vm, recv, index) -> i64` in
`../../../vm/src/jit/helpers.rs` that calls `cratonvm_native_collections::jit_arraylist_get` and
falls back to the generic dispatcher on `None`, bound guard-free like `hashmap_get`:
`DirectHelperTable::arraylist_get` / `arraylist_size` (`../../../jit/src/direct_helpers.rs`, plus
both `DirectHelperTable { .. }` literals in `../../../vm/src/jit/helpers.rs`, which spell every
field, so the field cannot land alone), the recognition arm next to the
`java/util/HashMap` one in `lib.rs`, refused under `--jdk-only` through
`direct_native_helper_for_impl`. It removes `jit_invoke_virtual_mic*`,
`try_jit_site_cached_native_dispatch`, `deadrecv_check` and `forward_jit_*` -- about a
quarter of the samples above -- and keeps `al_fast_state`'s checked reads. The inline
intrinsic removes both.

## Status after wave 5 (`typecheck5`, `../../../vm/src/jit/helpers.rs`)

Re-profiled `SpliceCastProbe 20000000` on the wave-4 binary (`diag/sampler.py 0.6 3`, 1821
busy samples). With vm4's ArrayList leaf in place, the helper-side self time is:

| Frame | Self time |
|---|---|
| `try_jit_site_cached_native_dispatch` | 10.5 % |
| `jit_invoke_virtual_mic_body` | 6.2 % |
| `forward_jit_reference_args` + `forward_jit_arg_at` | 3.6 % |
| `jit_invoke_virtual_mic` | 2.3 % |
| `try_jit_arraylist_leaf` | 1.6 % |
| `note_site_cached_native_hit` | 0.9 % |

The heap side outweighs it:

| Frame | Self time |
|---|---|
| `ZgcRealHeap::is_object_address` | 11.0 % |
| `VmHeap::is_object_address` | 3.9 % |
| `get_array_element` (two layers) | 9.7 % |
| `coerce_field_value_for_slot` | 4.6 % |
| `al_fast_state` | 4.4 % |
| `load_and_forward_inner` | 4.1 % |

`deadrecv_check` is still 2.5 %.

Done here. Both are small, and both keep the path's behaviour the same:

- `try_jit_site_cached_native_dispatch` probed the thread-local `NATIVE_SITE_CACHE` twice
  per call for the same key: once for the cached refusal, and once more for the warm hit.
  It now probes once and reuses the copy. Nothing between the two probes can write the
  map; the only work there is the receiver's heap-membership read.
- `cv_trace_match` now tests the latched `CRATONVM_TRACE_CLASSVALUE` bool before its two
  string compares. Every compiled `get` call (`List.get`, `Map.get`) reaches it on each
  `jit_invoke_virtual_mic`.

Left, with the reason for each:

- **Receiver validation done several times.** The dispatch validates the receiver with
  `is_object_address`, then `jit_arraylist_get`/`al_fast_state` validate it again
  (`class_id_of_object`, `get_fields_typed`, `array_shape`, the element read). Passing the
  validated `ObjectRef` and its class id into the probe needs a new signature in
  `native-collections`, so it is not this lane's to change.
- **`forward_jit_reference_args`.** It runs a `load_and_forward` per reference argument
  on every call. Skipping it safely needs a "nothing moved since" epoch from the
  collector.
- **`deadrecv_check`.** It is in `vm_exec.rs`.
- **The JIT intrinsic for exact-`ArrayList.get`/`size`** (proposed fix 1): still the
  large win. It is JIT-side.
- **`Integer` boxing** (fix 3): the allocation pages.

The page stays PARTIALLY FIXED.

## Status after wave 8 (`arraylist8`, proposed fix 1 -- single-pass tier landed)

### What changed

The intrinsic is a guarded PREFIX of the site's unchanged virtual/interface dispatch
rather than a `JitDirectCall` sentinel. The single-pass emitter recognises the site
from its own `JitInvokeInfo` (`java/util/ArrayList|List.get(I)Ljava/lang/Object;`,
`java/util/ArrayList|List|Collection.size()I`, invoke kind virtual or interface),
emits the fast path, and lets every decline edge fall onto the first byte of the
ordinary MIC/PIC dispatch, which then runs `native_al_get` / `native_al_size` exactly
as before (the vm4 leaf included). This departs from the wave-5 contract in one
respect, on purpose: nothing is registered at any compile door, so there is no
"registered at one door, refused at the other" failure (the StringBuilder history on
`string_intrinsic_declines_to_a_call`), the site keeps its MIC/PIC, and the
optimizing tier -- which never runs this emitter -- is byte-for-byte unchanged. The
contract's names all exist and are what the emitter uses:

* `../../../jit/src/intrinsic_catalogue.rs`: region `ARRAYLIST_ACCESS` (`ArrayListGet`,
  `ArrayListSize`; `LAST` moved), `pub struct ArrayListFieldLayout` (the five contract
  fields plus the two field indices, the two legacy cell-TAG offsets and
  `min_num_slots`), `ArrayListFieldLayout::new(data_idx, size_idx, class_id)` with every
  refusal (class id 0, `CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS=1`, `CRATONVM_DBG_ALTRACE`,
  narrow oops, an armed ZGC read barrier, a registered compact width other than 4 /
  8), `try_resolve_arraylist_intrinsic(..)`, `arraylist_intrinsics_disabled()`.
* `../../../jit/src/lib.rs`: `StringFieldLayout::array_list` + `with_array_list`.
* `vm/src/runtime/interpreter/dispatch_static.rs::resolve_string_field_layout`:
  resolves `java/util/ArrayList` + `elementData:[Ljava/lang/Object;` + `size:I` under
  the same read lock; `None` under `--jdk-only` (the rows are `Bridge`). All three
  compile doors (method entry, callee, OSR) already take their layout from this one
  function, so the OSR door needed no edit of its own. No redefinition screen:
  `ArrayList` is `redefine_immune_synthetic_collection_native`, so the native answers
  after a retransform today and so does its inline fast path.
* `jit/src/x64/op_invoke.rs::try_emit_arraylist_access_prefix`, called at the top of
  the dispatch arm of `walk_invoke_instance`; the hit edge joins at that arm's result
  push. The emitted sequence is the page's wave-5 design plus three screens the native
  also applies: `num_slots >= max(idx)+1` and the legacy cell tags (`Int` for `size`,
  `Object` for `elementData`), and `size >= 0`. Never a deopt; never an exception.

### Tests

`../../../jit/tests/r9w8_arraylist8_arraylist_access.rs` (executed code, fake heap shapes):
legacy and compact hits for `get` and `size`; declines for another class id, null
receiver, an `ArrayList[]` receiver, null and `int[]` `elementData`, index `-1`,
`size`, `size <= i < length`, `length`, the `values()`-view marker, a negative size,
mistagged legacy cells, a short instance; the no-layout control; the resolver's site
table; the kill switch.

### Measured before (wave-7 binary, this box, 2026-09-18)

`AlProbe` (scratch probe: 400 000 x `for (i < l.size()) s += l.get(i)` over a
16-element `ArrayList<Integer>`, round 4):

| mode | CratonVM (default) | `CRATONVM_C2_SUPERSEDE=0` | HotSpot |
|---|---:|---:|---:|
| `ArrayList`-typed `get`/`size` | 3356 ms | 3404 ms | 5 ms |
| `List`-typed, `size()` hoisted | 1844 ms | 1771 ms | 4 ms |

`CratonBench hashmap`: 6353 ms. No after-number is possible without a build; the
integrator's build should re-run `AlProbe get`/`list` and the page's own
`SpliceCastProbe` / `CratonBenchC2 pipeline` recipe, with and without
`CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS=1`, and read `CRATONVM_DBG_JITC=1` for
`arraylist-intrinsic` lines to see which bodies took it.

### Remaining

1. **The optimizing tier.** An `[c2]` body's `List.get` is an `Op::Call` lowered by
   `ir_lower.rs`, which this lane does not own; it is unchanged. Cross-lane request
   (exact shape) in `../../internal/jit-review-r9/NOTES-w8-arraylist8.md`.
2. **Fix 3** (`Integer` boxing through the helper allocation path) -- the allocation
   pages.

## Status after wave 8b (`wire8b`)

* The IR-tier half of proposed fix 1 (arraylist8's cross-lane request 2) was NOT
  attempted: it is a new hand-encoded ~20-instruction guarded prefix inside
  `ir_lower.rs`'s `Op::Call` arm, with its own decline/hit control flow around the
  call's post-call sequence, and it cannot be verified without a build. It stays open
  exactly as `NOTES-w8-arraylist8.md` request 2 specifies.
* arraylist8's optional request 3 landed: the interpreter's eager first-call door
  (`../../../vm/src/runtime/interpreter.rs`, typecheck table) now interns its `instanceof`
  targets with `intern_typecheck_target_with_finality` and the
  `jit_bridge::jit_class_is_final_for_typecheck_in` verdict, like the three
  `CompileRequest` doors. (That is the final-user-class `instanceof` fast path, not
  this page's ArrayList item.)

## Status after wave 9 (`irl9`, the optimizing-tier half of proposed fix 1)

### Why it matters (w8b, same session)

`AlProbe` (arraylist8's probe; 400 000 x a 16-element `ArrayList<Integer>` loop), round 4:

| mode | default (C2 body runs) | `CRATONVM_C2_SUPERSEDE=0` (C1 body, has the prefix) |
|---|---:|---:|
| `list` (`List`-typed `get`/`size`) | 1 704-1 900 ms | 28-31 ms |
| `get` (`ArrayList`-typed) | 30 ms or 3 277 ms (which body `main`'s OSR code binds is a race) | 33 ms |

The wave-8 single-pass prefix is a 60-100x win, and it is lost the moment the method tiers up,
because the `[c2]` body's `List.get` is an `Op::Call` lowered by `ir_lower.rs`.

### What changed (`../../../jit/src/ir_lower.rs`)

* `Lowerer::try_emit_ir_arraylist_access_prefix`, called first in the `Op::Call` arm: the same
  guarded sequence as `x64/op_invoke.rs::try_emit_arraylist_access_prefix` (layout epoch guard,
  null, `KIND_TAGS == Object`, exact class id, compact/legacy fields with the legacy slot-count
  and tag screens, `size >= 0`, non-null reference-array `elementData`, unsigned index vs
  size and length, the `view_source_in` spare-slot marker), using only this lowerer's scratch
  registers (RAX, RCX, RDX, R10, R11 -- the single-pass R8/R9 are allocatable here). Every
  decline edge lands on the unchanged call lowering (MIC/PIC, direct or dispatch). The hit
  writes the result exactly as the call route's `store_rax` would at run time (home word, plus
  the resident register when the value has one) and jumps to `Lowerer::call_prefix_join`,
  which `lower_data_node_tracked` patches after the node's whole lowering (the call arm has
  several early `return`s). Refused at compile time for: no layout, the kill switch
  `CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS=1`, `invokespecial`, an armed ZGC read barrier, a
  mismatched argument count / result type, a carried operand, a carried or dropped-home result.
* The layout reaches the lowerer through a new entry,
  `ir_lower::lower_inner_with_array_list(.., array_list: Option<ArrayListFieldLayout>)`
  (`lower_inner_with_cycle_cells` now forwards `None`, byte-identical). It is THIS compile's
  `StringFieldLayout::array_list`, never `ir::published_string_layout`.
* **Not in this lane's files:** `lib.rs::ir_tier` must call the new entry with
  `resolved_string_layout.and_then(|l| l.array_list)` -- `NOTES-w9-irl9.md` cross-lane
  request 2. Until it does, the IR lowering is unchanged byte for byte.

### Test

`ir_lower.rs` unit test `r9w9_ir_arraylist_get_and_size_answer_inline_or_dispatch`: lowers
`static Object f(ArrayList, int) { return l.get(i); }` and `static int g(ArrayList) { return
l.size(); }` through `lower_inner_with_array_list` and EXECUTES them over fake legacy and
compact receivers (hits for every in-range index and both `size()` shapes), plus declines
that must reach the dispatch stub: `index == size`, `-1`, `index == length`, another class id,
a null `elementData`, a negative size, a view marker in the spare slot, an array receiver;
and the same body lowered with no layout dispatches every time.

### How to verify after the build

```bash
for m in get list; do "$VM" --java-home "$JH" -cp <scratchpad>/al8 AlProbe $m; CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS=1 "$VM" --java-home "$JH" -cp <scratchpad>/al8 AlProbe $m; done
CRATONVM_DBG=ir-stages "$VM" ... AlProbe list 2>&1 | grep "arraylist-intrinsic n"   # the IR sites that took the prefix
```

## Status after wave 9 (evict9): the single-pass `List`-typed path, the C2 downgrade, and the pin

### The single-pass `invokeinterface` path already engages

The lane brief described `AlProbe list` (~870 ms vs HotSpot 4 ms) as a missing single-pass
`List`-typed (invokeinterface) prefix. It is not missing: on w8b,
`CRATONVM_DBG_JITC=1 ... AlProbe list` prints

```text
[cratonvm-jitc] arraylist-intrinsic AlProbe.sumList:(Ljava/util/List;)I java/util/List.size()I @pc=3 guard_class_id=63
[cratonvm-jitc] arraylist-intrinsic AlProbe.sumList:(Ljava/util/List;)I java/util/List.get(I)Ljava/lang/Object; @pc=19 guard_class_id=63
```

and the single-pass body is fast. What makes the default slow is the C2 supersede:
`c2-supersede published AlProbe.sumList ... c1=6135 c2=5701 outcome=changed` replaces the
prefix body with an IR body that calls the MIC helper per `get`. Same session, w8b (ms):

| run | `list` r0..r4 | `get` r0..r4 |
|---|---|---|
| default (C2 body) | 876 / 925 / 1511 / 2017 / 1839 | ~3 000 (wave 8) |
| `CRATONVM_C2_SUPERSEDE=0` (single-pass body) | 33 / 25 / 25 / 25 / 25 | 53 / 38 / 38 / 39 / 39 |
| `CRATONVM_C2_SUPERSEDE=0 CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS=1` | 1162 / 932 / 1442 / 2026 / 2001 | 2171 / 3194 / 3522 / 3837 / 3681 |
| HotSpot 25 | 4 | 5 |

The page's own recipes show the same downgrade: `CratonBenchC2 pipeline` 208 / 223 ms
default vs 45 / 45 ms single-pass; `SpliceCastProbe` 706-730 ms default vs 90-96 ms in the
runs where the single-pass `step` body won the publication race (its C1 and C2 compiles
race; 3 of 4 `SUPERSEDE=0` runs still ended on the IR body at ~715 ms).

### What changed (`../../../jit/src/lib.rs`)

* **The pin.** `arraylist_prefix_pins_single_pass` (next to `single_pass_only_lowering_for`)
  joins the optimizing-tier admission conjunction right after the String pin, and the
  printed admission verdict names it. It asks exactly what the emitter asks: a resolved
  ArrayList layout on this compile, an `invokevirtual`/`invokeinterface` site
  `try_resolve_arraylist_intrinsic` answers, and no armed ZGC read barrier. Opt out with
  `CRATONVM_JIT_NO_ARRAYLIST_PIN=1` (new flag; INVENTORY row in `NOTES-w9-evict9.md`).
  It is further gated by `const IR_TIER_LOWERS_ARRAYLIST_PREFIX: bool = false`.
* **irl9's cross-lane request 2 applied**: `ir_tier` now calls
  `ir_lower::lower_inner_with_array_list(.., resolved_string_layout.and_then(|l| l.array_list))`,
  so the IR prefix is armed. (Also irl9's request 1: the IR typecheck table interns with the
  finality verdict.)
* Test: `lib.rs` `r9w9_evict9_tests::the_arraylist_pin_fires_only_on_a_prefix_site_with_a_layout`.

### The decision left to the integrator

With both landed, the pin keeps the MEASURED single-pass body (25 ms on `AlProbe list`) and
irl9's IR prefix runs only under `CRATONVM_JIT_NO_ARRAYLIST_PIN=1`. A/B on the built binary:
`AlProbe list|get`, `CratonBenchC2 pipeline`, `SpliceCastProbe`, each default vs
`CRATONVM_JIT_NO_ARRAYLIST_PIN=1`. If the IR body with the prefix is no slower, set
`IR_TIER_LOWERS_ARRAYLIST_PREFIX` to `true` (one line in `lib.rs`), which retires the pin.

## Status after wave 10 (collect10): fix 3, the `Integer` boxing

### Where the boxing cost actually is (w9b, this box, 2026-09-19)

Probe `C:\craton\jitr9-probes\collect10\BoxProbe.java` (10 M iterations per round, round 3,
ms; w8b / w9b interleaved, HotSpot 25 for reference):

| mode | body | w8b | w9b | HotSpot |
|---|---|---:|---:|---:|
| `box` | `sink[i & 1023] = Integer.valueOf(i + 1000)` (escaping, out of cache) | 861 | 882 | 26 |
| `put` | `m.put(i & 127, i & 127)` (CACHED boxes, no allocation) | 2605 | 2528 | 15 |
| `get` | `s += m.get(i & 127)` (cached key box) | 1631 | 1430 | 11 |
| `bu` | `Integer a = i + 1000; s += a.intValue();` (box never escapes) | 641 | 657 | 2 |

`CratonBench hashmap` (10 M put + 10 M get): w8b 4719 / 4694 ms, w9b 4626 / 4615 ms.
`AlProbe get` 29-30 ms both; `AlProbe list` w8b 922 ms, w9b 26 ms (the wave-9 pin).

Sampler (`diag/sampler.py`, w9b), self time:

* `put` (cached boxes): `safe_native_call_impl` 18.8 %, `native_integer_value_of` 17.7 %,
  `jit_integer_value_of_direct` 12.0 %, `is_object_address` 8.2+3.1 %, `hash_one` + `sip` 8.6 %,
  `try_hm_int_fast_put` 4.9 %. The CACHED range (`-128..=127`) is the most expensive
  `valueOf` there is: `jit_integer_value_of_direct` sends it through the full
  `safe_native_call_prevalidated_objects` funnel to `native_integer_value_of`, which takes
  two global mutexes over SipHash `HashMap<usize, _>` tables (`integer_cache_high()` via
  `integer_cache_bound`, then `integer_cache()`). (`hash_one<RandomState, ObjectRef>` in the
  profile is the linker-folded `hash_one<usize>` of those two tables.)
* `CratonBench hashmap`: `jit_integer_value_of_direct` 16.5 % self (the helper prologue:
  `contain`, `note_jit_boundary`, SATB flush, `integer_cache_high_for`'s mutex + SipHash
  lookup, two TLS caches, the redefinition probe), the TLAB closure 6.9 %, the collection
  natives ~10 %, field access through the heap trait ~12 %.

### What changed (this lane's files)

* **IR-tier box/unbox fold** (`../../../jit/src/ea_ir_bridge.rs`, `ir_fold_unbox_of_fresh_box`, run
  from `ir_fold_null_checks_on_fresh_allocations`, which `lib.rs` already calls on every IR
  compile before `ir_optimize`). An `Op::Call` to `Integer.valueOf(I)` / `Long.valueOf(J)`
  whose every VALUE reader is an `Op::Unbox { IntValue | LongValue }` of the same width, as
  its receiver, is deleted: each unbox is forwarded to the boxed primitive and the memory
  chain is spliced over both. Refused when any deopt that can still happen would resume with
  the box live (`ea_consumable_snapshot_names`, the scalar-replacement planner's own test,
  with the call and its unboxes treated as gone). Identity is unobservable by construction
  (no compare, store, lock or call sees the box), so cached-vs-fresh does not matter.
  Kill switch `CRATONVM_NO_IR_BOX_UNBOX_FOLD=1`. Targets `bu`-shaped code (and the same
  shape after inlining a generic helper); expected `bu` ~650 ms -> single-digit ms.
  Tests: `ea_ir_bridge.rs` `r9_ea_bridge_tests::{integer_value_of_read_back_only_by_int_value_folds_to_its_argument,
  a_box_live_at_a_consultable_snapshot_is_not_folded, a_box_with_any_other_reader_is_not_folded,
  long_value_of_and_nested_boxes_fold}`.

### Filed as exact cross-lane edits (not this lane's files)

`../../internal/jit-review-r9/NOTES-w10-collect10.md`, "Cross-lane requests":

1. `../../../native-builtins/src/lang_math.rs`: `pub fn integer_cache_peek(vm_identity, value)`, and
   `rustc_hash::FxHashMap` for the two per-VM tables every `Integer.valueOf` reads
   (`ScopedIntegerCache`, `INTEGER_CACHE_HIGH`).
2. `../../../vm/src/jit/helpers.rs` (`jit_integer_value_of_direct_body`): serve a cached value from
   `integer_cache_peek` before the `safe_native_call` fallback. Expected to remove most of
   the `put`/`get` rows' `safe_native_call_impl` + `native_integer_value_of` share (~35 % of
   `BoxProbe put`).

### Still open

The ESCAPING box (`CratonBench hashmap`'s keys and values, `BoxProbe box`): every
`valueOf` beyond the cache is a helper call that allocates through the TLAB path. The fix
is a JIT-emitted inline allocation (`emit_inline_tlab_new` of `java/lang/Integer` + the
field store, with the helper only for the cached range), in `x64/op_invoke.rs` / `lib.rs`
(single-pass) and `ir.rs` / `ir_lower.rs` (IR) -- none of them this lane's files.

## Status after wave 10b (wire10b)

collect10's cross-lane edits landed:

* `../../../native-builtins/src/lang_math.rs`: `pub fn integer_cache_peek(vm_identity, value: i32)`
  (next to `integer_cache_high_for`) and a new `Long` twin `pub fn long_cache_peek(vm_identity,
  value: i64)`. Both are thin wrappers over the existing `canonical_wrapper_if_cached` (its
  `I` / `J` arms read the same tables the natives' fast paths read, and already treat the
  `Integer` store's length as the bound), not a second copy of the read. Test:
  `tests::r9w10b_cache_peeks_answer_the_natives_object_and_nothing_else`.
* `../../../vm/src/jit/helpers.rs`: `jit_integer_value_of_direct_body` answers a filled cache slot from
  `integer_cache_peek` before the `safe_native_call` fallback (guarded by `!beyond_cache`);
  `jit_long_value_of_direct_body` does the same for `-128..=127` via `long_cache_peek`. A cold
  slot still takes the native, which fills it.
* Fx maps: `INTEGER_CACHE_HIGH` (read on every JIT `Integer.valueOf` through
  `integer_cache_high_for`) is now `parking_lot::Mutex<rustc_hash::FxHashMap<usize, i32>>`.
  `ScopedIntegerCache` was NOT converted: `integer_cache()` is passed to the generic
  `scan_one_cache` / `update_one_cache` (and a third site), whose parameter type is
  `OrderedPlMutex<std::collections::HashMap<usize, C>>`; switching it means generalizing those
  helpers over the hasher for the whole value-cache family -- a separate change.

After the build, A/B against w9b: `BoxProbe put` and `BoxProbe get` (expected: most of the
`safe_native_call_impl` + `native_integer_value_of` share gone).

## Integrator measurement after wave 10 (w10 binary, 2026-09-19)

`BoxProbe` (C:/craton/jitr9-probes/collect10), round 3, interleaved with w9b, 3 reps:
- `bu`: w9b 651-668 ms, w10 4 ms (the fold), w10 with `CRATONVM_NO_IR_BOX_UNBOX_FOLD=1` 604-657 ms.
- `put`: w9b 2 546-2 572 ms, w10 1 629-1 645 ms.

Open: escaping boxes and the `HashMap` native side (HotSpot: `put` 15 ms, `get` 11 ms).

## Status after wave 11 (collect11)

### Profile of the w10 binary (sampler, `BoxProbe`, 2.5 s after 1.5 s warm-up, `CRATONVM_JIT_RETIRE_CELL=1`)

Timings this session (round 3): `box` 900 ms, `put` 1 743 ms, `get` 1 181 ms.

* `box` (escaping, out-of-cache): `jit_integer_value_of_direct` 30.9 % self, TLAB closure
  15.5 %, `is_object_address` 8.9+2.8 %, `aastore_store_is_refused` 4.6 % +
  `aastore_element_assignable` 2.1 % + `jit_aastore_type_check` 1.6 % + `element_type_of`
  1.3 % (the `Object[]` store check), `plan_tlab_object_shape_at` 3.5 %,
  `record_object_ref_payload_slow` 3.4 %, compact-field write path ~10 %.
* `put` (cached boxes): `jit_integer_value_of_direct` 18.6 %, **`canonical_wrapper_if_cached`
  13.3 %**, `safe_native_call_impl` 10.8 % (the `HashMap.put` native, not `valueOf`),
  `try_hm_int_fast_put` 7.3 %, **`hash_one<RandomState>` 3.4 % + `sip::write` 2.2 %**.
* `get`: `jit_integer_value_of_direct` 15.1 %, `try_hm_int_fast_get` 12.0 %,
  **`canonical_wrapper_if_cached` 10.3 %**, `hashmap_string_node_cache_get_object` 8.6 %,
  `fast_unbox_primitive_wrapper` 5.9 %, **SipHash ~3.7 %**.

So after wave 10b the cached `valueOf` no longer went through `safe_native_call`, but each
one still took two mutexes and a SipHash probe: `integer_cache_high_for` (mutex + Fx probe)
for the `beyond_cache` test, then `integer_cache_peek` -> `canonical_wrapper_if_cached` ->
`OrderedPlMutex` (lock-order tracking) + SipHash `HashMap<usize, Vec<..>>`. The out-of-cache
`box` row paid the first of those on every call too.

### What changed (`../../../native-builtins/src/lang_math.rs`)

* `INTEGER_CACHE`'s per-VM store is now an `IntegerCacheTable { vm_identity, high, slots:
  Box<[AtomicUsize]> }` (`lang_math.rs` ~3108), leaked once per VM identity (the `Vec` it
  replaces was never removed or resized either). The `OrderedPlMutex<HashMap>` stays, as the
  directory the tables are filed in (no lock-ratchet change).
* `static INTEGER_CACHE_LAST: AtomicPtr<IntegerCacheTable>` + `integer_cache_table(vm)`
  (~3191): one `Acquire` load and one compare in steady state; a different/unknown VM falls
  back to the locked directory and repoints the pointer. Never answers another VM's table
  (the table carries its own `vm_identity`).
* Readers moved onto it: `integer_cache_high_for` (the JIT helper's `beyond_cache` test),
  `integer_cache_peek` (the JIT helper's cached arm; now skips the descriptor match too),
  `canonical_wrapper_if_cached`'s `I` arm, `integer_cache_bound`'s steady state and
  `native_integer_value_of`'s fast path. Fills are a `compare_exchange(0, obj)` (`Release`),
  so two racing misses still agree on one canonical box without the lock; GC scan/remap got
  dedicated `scan_integer_cache` / `update_integer_cache` (remap is a per-slot CAS old->new).
* `integer_cache_bound` files the table under the memo lock exactly as before (memo -> cache
  order unchanged) and takes the memo's `high` FROM the filed table, so the memo, the table
  length and `integer_cache_high_for` cannot disagree. The `try_reserve_exact` fallback to
  `high = 127` is kept.
* Tests (`lang_math.rs` test module): `r9w11_integer_cache_fast_table_is_vm_scoped_under_interleaving`
  (two VMs with different bounds, lookups alternating so the last-table pointer flips every
  call; peek / probe / bound / native all agree, no cross-VM answer),
  `r9w11_integer_cache_table_install_keeps_the_first_box` (CAS publish, bounds incl.
  `i32::MAX`/`MIN`). Existing `r9w10b_cache_peeks_*`, `the_widened_integer_region_is_both_scanned_and_remapped`
  and the `canonical_wrapper_if_cached_*` tests cover the scan/remap and read contracts.

Expected (not measurable here: the w10 binary predates the edit): removes the
`canonical_wrapper_if_cached` + SipHash share of `put`/`get` (~17-19 % / ~14 %) and the
`integer_cache_high_for` mutex inside `jit_integer_value_of_direct` on every row including
`box`. Integrator A/B: `BoxProbe put|get|box` and `CratonBench hashmap`, w10 vs w11
interleaved.

### Still open (none of it in this lane's files)

1. **Escaping box allocation** (`box`, `CratonBench hashmap` keys/values): the fix is a
   JIT-emitted `valueOf` -- `if (v - (-128)) <u (high + 129)` take the helper (cached arm),
   else `emit_inline_tlab_new` / `emit_inline_tlab_new_ir` of `java/lang/Integer` and a
   32-bit store of `v` at the `value` field's (compact or legacy) offset, with the helper as
   the TLAB-exhausted slow path. Needs `x64/op_invoke.rs` + `x64/objects.rs` (single pass),
   `ir.rs` / `ir_lower.rs` / `runtime_lowering.rs` (IR), and the VM to hand the compile the
   `Integer` class id, its layout and the latched `high` (`integer_cache_high_for`; the
   compile must refuse the inline arm while that is `None`, and a later `-D` change cannot
   move it, see `integer_cache_bound`). The layout question is the one that bit
   `FjpProbe` (compact vs legacy `value` slot, see the comment in
   `jit_integer_value_of_direct_body`).
2. **`Object[]` store check** in `box` (~9.6 %: `aastore_store_is_refused`,
   `aastore_element_assignable`, `jit_aastore_type_check`, `element_type_of`): a store into
   an array whose element class is `java/lang/Object` needs no subtype check; worth an
   early-out in `../../../vm/src/jit/helpers.rs`.
3. The `HashMap` native side (`safe_native_call_impl`, `try_hm_int_fast_*`,
   `hashmap_string_node_cache_get_object`, `fast_unbox_primitive_wrapper`): lane chm11.
4. `LONG_CACHE` (and the other four fixed caches) still read through `OrderedPlMutex` +
   SipHash; `long_cache_peek` would take the same table treatment if a `Long`-heavy profile
   shows it.

## Integrator measurement after wave 11 (w11 binary, 2026-09-19)

`BoxProbe`, round 3, interleaved with w10, 3 reps. This includes collect11's lock-free
Integer-cache table and chm11's overlay-before-String-cache reorder in `jit_hashmap_get_direct_body`:
- `box`: w10 920 / 818 / 813 ms; w11 759 / 762 / 789 ms.
- `put`: w10 1 767 / 1 655 / 1 639 ms; w11 1 225 / 1 224 / 1 230 ms.
- `get`: w10 981 / 985 / 988 ms; w11 664 / 667 / 673 ms.

`CratonBench hashmap`: w10 4 503 / 4 488 ms, w11 4 157 / 4 109 ms. Still open: escaping-box
allocation and the native call itself (HotSpot: 26 / 15 / 11 ms).
