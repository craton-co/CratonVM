> **j27 wave 8 (lane W8FFM, 2026-10-28): REVIEWED, PENDING HOST VERIFICATION.** No code change. `member_name.rs` `vh_static_slot_of`: the key is read before the resolution (a `<clinit>` that collects moves the generation, so its line is never served), a line is filled only for an initialized holder, a hit re-checks `is_class_initialized` and the unload count, and the switch is read only on a fill. One known limit, perf only: the VM's cell filing (`vh_static_note_target`) rides the memo FILL, so a line filled while the handle's own `static final` holder was still in `<clinit>` (the handle used inside that `<clinit>`, its target class already initialized) is served afterwards without a note, and that thread never files the cell; another thread's miss, or the next generation move, files it. Correctness is unaffected (an unfilled cell is the call). If the host shows a static site that stays on its call after warm-up (`CRATONVM_DBG_JITC=1`: `vh-const-static (ir)` but no `vh-const-static filed`), this is the first suspect.
>
> **j27 wave 4 (lane VHCARD, 2026-10-28): STILL RELEVANT, already IMPLEMENTED IN CODE (FFMVH); no code change this wave, PENDING HOST VERIFICATION.**
> - **What VHCOMP's compiled cells cover:** a compiled constant static site no longer reaches the native, so the memo does not matter there.
> - **What still reaches the native, and so still uses the memo:** the interpreter, a site the compiled plan declines (`LazyInitializingVarHandle`, a non-constant handle, compiled before its cell was registered, `CRATONVM_JIT_VH_CONST_STATIC=0`), and the first calls before tier-up.
> - The memo's fill site also drives VHCOMP's cell filing (`vh_static_note_target`). That call runs with the memo switch off too.
> - **Measured** on Windows, P-cores, `R21AtomicsRows -Dr21.timeIters=200000`, `time static-vh-int-xadd` ns/op, 5 interleaved reps, medians:
>
>   | arm | memo ON (default) | `CRATONVM_VH_STATIC_SLOT_MEMO=0` |
>   |---|---|---|
>   | JIT, default | 9 | 14 (noise; the compiled cell serves it) |
>   | `CRATONVM_JIT_VH_CONST_STATIC=0` | 1463 | 1968 |
>   | `--nojit` | 1244 | 1297 |
>
> - `fact static-vh-int-xadd 80000` in every arm.
> - **Keep it default ON.** The memo cuts the native's share by about 25% where the native runs.

# Proposal: memoise a static-field `VarHandle`'s `(ClassId, slot)` per handle

**Status:** IMPLEMENTED IN CODE (j27 wave 1 follow-up, lane FFMVH, 2026-10-28), PENDING HOST VERIFICATION.
- Code: `vh_static_slot_of` in `native-builtins/src/lang_invoke/member_name.rs`.
- Switch: `CRATONVM_VH_STATIC_SLOT_MEMO`.
- The verify recipe is in the parent page's status block.

**Area:** `native-builtins/src/lang_invoke/member_name.rs`: the `VH_KIND_STATIC` arms of the `VarHandle` get / set / CAS / getAndAdd / getAndSet / bitwise natives, and `vh_static_slot`.

## Evidence (by reading)

Every static-field `VarHandle` access through the natives goes through the same steps, on every call:

* it clones the row's `class_name` and `field_name` (two `String` allocations);
* it calls `vh_static_slot`, which:
  * walks `inherited_static_field_holder`,
  * runs `vh_initialize_static_holder` (or `is_class_initialized` / `ensure_class_initialized` by name),
  * and resolves `static_field_index_by_name`.

The access itself is then one `static_field_hw_update`. See the parent page, `jr26-static-varhandle-rmw-is-a-native-call-FIXED-20261029.md` (about 1.6 µs per `getAndAdd` against HotSpot's 12 ns).

## Proposal

Once the holder reads initialized, keep its `(ClassId, slot)` in a per-thread memo, the way `SEG_VH_PLAN_MEMO` / `P67_SHAPE_MEMO` do. Key it by VM identity, identity hash, address and `VH_META_GENERATION`.

Rules for the memo:

* Fill it only after `vh_static_slot` has answered for an initialized holder, so the `<clinit>` and `LazyInitializingVarHandle` frame paths stay exactly as they are.
* Default ON, with an `=0` kill.

How to tell whether it worked:

* A/B the `time static-vh-int-xadd` row of `tools/probes/R21AtomicsRows.java`.
* The `fact` rows must stay HotSpot's.

The compiled static constant-`VarHandle` plan on the parent page remains the real fix. This only cuts the native's own share.
