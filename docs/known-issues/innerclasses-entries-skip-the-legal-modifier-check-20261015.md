> **MOVED (20261019, zt round orchestrator):** out of the ZGC set: its open part belongs to another owner (zt t5/pg3 audit, `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md`). Former name `docs/known-issues/innerclasses-entries-skip-the-legal-modifier-check-20261015.md`.

# ZGC round zs, wave s5: review residuals (lane s6/rv4)

> **STATUS (2026-10-19, zt t5/pg3): OTHER-OWNER (class-file parsing; one JIT comment).** 1, 4, 5, 6 FIXED, 3 DECIDED, 8 CLOSED by reading. Open and not ZGC: 9 (`InnerClasses` entries are masked but not checked like HotSpot's `verify_legal_class_modifiers`; a `defineClass` row with an inner entry `0x0200` decides), 7 (the bt2 comment in jit/src/ir_lower.rs `zgc_fresh_receiver_stores`, an opt-in arm), 2 (a `Box<str>` key per `layer_module_of_package`, cost only). Move to `docs/known-issues/innerclasses-entries-skip-the-legal-modifier-check-20261015.md`.

> **STATUS (2026-10-19, zt t3/nd): item 2 (a `Box<str>` key per `layer_module_of_package`) left OPEN: an allocation-free probe needs the map re-keyed (`ClassRealm::layer_module_packages` is `FxHashMap<(u32, Box<str>), usize>`, read by `vm_exec_zs5nd5_tests` too) and the cost has never been measured; cost only. Report: `docs/internal/zgc-round-zt-20261018/t3-nd-report.md`.**

> **STATUS (2026-10-19, zt t3/fp): item 8 CLOSED BY READING; item 9 open (HotSpot row).** The reconstituter writes a class's `InnerClasses` from the stored row (`write_class_attributes`, vm/src/runtime/instrument_reconstitute.rs:239-257: `entry.access_flags` of `Class::inner_classes`), and every store of that row goes through `hotspot_stored_inner_classes` (classloading/src/class_manager.rs:11544 at define, :17878 at re-parse), so the written flags are HotSpot's stored ones. The raw-flags arm the page cites (`write_attribute`'s `Attribute::InnerClasses`, :425) serves only a decoded member / `Code` attribute table, where `InnerClasses` is not a legal attribute. No code change; the retransform row with an entry carrying `0x0040` still confirms it against HotSpot.

> **STATUS (2026-10-19, zt t2/rv): OPEN (items 2, 7, 8, 9) -- 1, 4, 5, 6 FIXED, 3 decided.** Checked on `67bc580ab8`. 1: FIXED (s7/nd6). 4: FIXED IN CODE by this lane (`ZgcConcurrentMarkController::log_cycle_outcome`, gc/src/zgc_concurrent.rs:779, called by both driver closures); row: `CRATONVM_ZGC_MARK_INLINE_DRIVER=0 RUST_LOG=zgc=debug` prints `ZGC concurrent mark cycle complete` again. 5: FIXED IN CODE by this lane (leftovers converted whenever a word carries the task tag, gc/src/zgc/mark.rs:4275; the tag is bit 63, never a heap address). 6: FIXED (t1/mk 890b242f9e: `array_splits=` / `array_tasks=` on the concurrent mark-end line). 3: DECIDED out of scope (G1 / Generational behaviour; zt rules). OPEN: 2 (cost only: `layer_module_of_package` still boxes the key per lookup, vm/src/vm/vm_exec.rs:40225; lane nd); 7 (lane ca: the bt2 comment in jit/src/ir_lower.rs `zgc_fresh_receiver_stores`); 8 (`instrument_reconstitute` writes the raw InnerClasses flags, vm/src/runtime/instrument_reconstitute.rs:431; the masking helper is `pub(crate)` in classloading, class_manager.rs:863, lane fp) and 9 (HotSpot probe) -- both need the retransform / defineClass rows the page names.

> **STATUS (2026-10-15, zs s7/nd6): item 1 FIXED IN CODE (unit-tested; no host row needed); items 2-3 unchanged (2 is cost only; 3 is G1/Generational scope); items 4-9 belong to other lanes.** `prune_layer_module_pins` now drops the package rows BEFORE it frees the weak handles, and `layer_module_of_package` resolves the handle under the map's read guard (`CRATONVM_ZGC_LAYER_MODULE_PRUNE_ROWS_FIRST`, default ON, `=0` = the s5 order). Tests `zs5nd5_tests::zs7nd6_a_pruned_row_is_gone_before_its_handle_is_freed` and its kill control (which observes the stale row at the free).

> **STATUS (2026-10-15, zs s6/rv4): OPEN.** This page holds what the adversarial review of wave s5
> left open. The review covered `git diff 717994a98c..fba9d1866d`: lanes mc3 (array split), bt2
> (fused gate at every site, fresh elision), ta (tail census, post-refusal log), ob4 (pause GcInfo
> Metaspace key), rp2 (class / InnerClasses flags) and nd5 (proxy Module, layer modules, upcall
> unwind).
>
> The review found no critical or high defect. It fixed one medium (nd5's upcall containment did
> not cover the uncaught report, which allocates on the same exhausted heap), one low regression
> (rp2: redefining a pre-50 interface was refused) and one nit. Those fixes are in
> `docs/internal/zgc-round-zs-20261014/s6-rv4-report.md`. Everything below is low or
> informational, found by reading, not reproduced. Line numbers are against `zs-s6-rv4`.

## nd5 (layer modules, upcall unwind)

1. **`prune_layer_module_pins` frees the weak handles before it drops their package rows**
   (`vm/src/vm/vm_exec.rs` `prune_layer_module_pins`, `layer_module_of_package`).
   - A JNI handle is the address of its `Box` (`vm/src/native/jni.rs` `add_weak` / `remove`), so a
     freed handle's address is reused by the next `add` / `add_weak`.
   - Between `refs.remove(module_h)` (under the JNI lock) and the `layer_module_packages.write()`
     retain, a reader that copied the handle out of the map can resolve it after the reuse, and
     `layer_module_of_package` would hand Java whatever the new handle names (the next record's
     LOADER handle is allocated first) as a `Module`.
   - Unreachable today, which is why it is low: the pruned rows are those whose Module the
     collector cleared, so their loader and every class of it are dead, and loader namespace ids
     are never reused (`allocate_loader_id` is monotonic), so no live class asks for that
     `(ns, package)` key.
   - **Close:** in the prune, retain the map first, then remove the handles; and in
     `layer_module_of_package` resolve the handle while the map's read guard is held (the only
     JNI-then-map order in the tree is none: record and prune release the JNI lock before taking
     the map).
2. **Every `getModule()` reaches `layer_module_of_package` through a `Box<str>` allocation of the
   package key** (pre-existing shape, now on the proxy path too). Cost only.
3. **F2 is not ZGC-gated**: a minted user-loader proxy Module goes to `record_layer_module_packages`
   on every collector, where G1 / Generational still take the permanent global root
   (`add_global_root`). The by-name table also rooted it, so nothing regresses, but the leak stays
   on those collectors (nd5's own page, item 2).

## mc3 (array split, driver spawn)

4. **`spawn_and_join_or_inline` drops the driver's outcome logging**
   (`gc/src/zgc_concurrent.rs`). `spawn`'s closure logs `ZGC concurrent mark cycle complete`
   (debug) and the `INCOMPLETE mark set` warning; the new closure stores the outcome only. Only on
   `CRATONVM_ZGC_MARK_INLINE_DRIVER=0`. **Close:** move the two log calls into a helper both
   closures call.
5. **The leftover task-to-base conversion is gated on the CURRENT split switch**
   (`gc/src/zgc/mark.rs` `ZMarkWorker::run`). A pool whose switch is turned off between cycles
   (`ZMarkCoordinator::set_split_arrays`, a test API) while a worker still holds chunk-task words
   from an aborted split cycle publishes the tagged words as bases, which a non-split drain then
   scans as an object at `0x8000_...`. Production pools never flip the switch. **Close:** convert
   whenever the worker pushed a task in the previous cycle (a per-worker flag), not by the switch.
6. **The concurrent pool's mark-end line has no `array_splits=` / `array_tasks=`** (mc3's own
   finding 10; its proposal page item 3).

## bt2 (fresh-receiver elision, opt-in)

7. **The fresh window's "no safepoint in the window" premise is not checked against value
   materialization** (`jit/src/ir_lower.rs` `zgc_fresh_receiver_stores`). A scalar-replaced VALUE
   whose `New` is scheduled before the receiver's `New` and which materializes at the store
   allocates inside the window. The soundness argument survives it (the overwritten field is
   null, so the SATB pre barrier owes nothing; an ageing collection there cards a promoted
   receiver), but the rule's comment claims the stronger premise. **Close:** the Q1 census of the
   bt2 proposal page should count windows whose store value is in `sr_map`, and the comment should
   rest on the null-old-value argument alone.

## rp2 (class flags)

8. **`instrument_reconstitute` writes the `InnerClasses` flags from the raw attribute**
   (`vm/src/runtime/instrument_reconstitute.rs` ~:431), while the class's own flags are written
   from the stored (masked) value (:172). HotSpot's `JvmtiClassFileReconstituter` writes both from
   the parsed (masked) values. Differs only for an entry with an unrecognized bit (none from
   javac). **Close:** write `hotspot_stored_inner_class_flags(entry.inner_class_access_flags,
   major)`, or the stored `Class::inner_classes` row.
9. **HotSpot runs `verify_legal_class_modifiers` on every `InnerClasses` entry** (with
   `_need_verify`); this VM masks the entry but does not check it. Pre-existing; a probe with an
   inner entry `ACC_INTERFACE` without `ACC_ABSTRACT` (version >= 50) decides it.

## Rows that decide these

- 1: a unit test with a loop of `record_layer_module_packages` / `prune` on one thread and
  `layer_module_of_package` on another, asserting a resolved object is always a `Module`.
- 4: `CRATONVM_ZGC_MARK_INLINE_DRIVER=0 RUST_LOG=zgc=debug` on any probe: the cycle line is
  missing on `zs-s6-rv4`, present on `717994a98c`.
- 8, 9: a retransform probe over a class whose InnerClasses entry carries `0x0040`, and a
  `defineClass` of an inner entry `0x0200`, both against HotSpot 25.0.4.
