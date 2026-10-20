# ZGC define paths and native doors after the zs/dev merge: residuals

<<<<<<< HEAD
> **STATUS (2026-10-19, zt t7/x4): item 1's fix CORRECTED IN CODE, PENDING BUILD/PROBE.** The t6 presize read `classes` by name on the loader's class, and a by-name read answers the most-derived declaration. A user loader declaring its own `classes` field therefore had ITS `ArrayList` grown, its `modCount` bumped and its monitor taken. An `ArrayList` subclass would also have run its own `add`. `presize_loader_classes` now resolves the slot from `java/lang/ClassLoader` and touches only an exact `java.util.ArrayList` (native-builtins/src/classloader.rs:7218, :7267). Tests: `zt7x4_a_user_loaders_own_classes_field_is_left_alone`, `zt7x4_only_an_exact_array_list_is_presized`. Rows unchanged (P1/P2 of `t6-nd4-report.md`). Report: `docs/internal/zgc-round-zt-20261018/t7-x4-report.md`.
=======
> **STATUS (2026-10-19, zt t7/cl): item 1 corrected and extended, FIXED IN CODE, PENDING BUILD/PROBE; its two residuals and item 3 carried to `zgc-zt7cl-define-path-and-class-name-residuals-20261019.md` (§1, §2); item 3's row unchanged.** t6/nd4's presize sat only in `classloader::define_class_via_full`. That function is the synthetic-JDK / `JavaLangAccess` backend: `register_classloader_natives` is reached only from `register_synthetic_overrides`. In both real-JDK modes, `ClassLoader.defineClass*` reaches `lang_system::native_classloader_define_class0/1/2` (native-builtins/src/lib.rs, the `defineClass0/1/2` `Bridge` registrations), and those never called `addClass` at all (t6/nd4 finding 14). So t6's deciding row `Zt6Nd4AddClassBoundary` printed `linkage=0` on both of its arms: the row was vacuous. t7/cl adds the upcall to those three doors, to `Unsafe.defineClass0` and to `--compatible`'s `Lookup.defineClass` door (`CRATONVM_DEFINE_ADD_CLASS`, default ON, `=0` kill, every collector: it moves both modes to HotSpot). It also runs the ZGC presize there before the reservation (`classloader::presize_for_add_class`). `Zt6Nd4AddClassBoundary` (both modes) is now the row that decides item 1, together with the new `Zt7ClAddClass`. The racing `addClass` between the presize and our upcall stays open. Holding the list's monitor across the define could deadlock, and reserving the slot shows a `null` in the list; the new page §1 has what would close it. Report: `docs/internal/zgc-round-zt-20261018/t7-cl-report.md`.**
>>>>>>> zt7-cl

> **STATUS (2026-10-19, zt t6/nd4): items 1, 4, 5 FIXED IN CODE, PENDING BUILD/PROBE; item 3 and the carried host items decided by named rows; zs2nd2 §4 and zs3nd3 §4 closed.** Item 1: the precondition holds by reading (a monitor follows its object across a move: the lock word is in the header, which every collector copies verbatim, and ZGC re-keys inflated monitors through `monitors.remap_after_gc` with its pointer map, gc/src/zgc.rs `collect_garbage`), so `define_class_via_full` now grows `ClassLoader.classes` under its monitor before the reservation (`presize_loader_classes`, `CRATONVM_ZGC_ADDCLASS_PRESIZE`, default ON on ZGC, `=0` kill); deciding row `Zt6Nd4AddClassBoundary` under `--compatible` (`linkage=0`, kill arm `linkage>0`). Item 4: the Spring lookup/replace-override builders' mint collects on ZGC (`CRATONVM_ZGC_SPRING_BUILDER_COLLECT`, default ON). Item 5: `DefinePhase::MirrorReserve` (`mirror-reserve` census row). Item 3 (off the default paths): deciding row `Zt1NdDefineRetry` under `--compatible` with `CRATONVM_CLASS_MIRROR_SLOT0_OVERLAY=1` (`linkage=0` means the collection relieves it; non-zero means park the wrapper). zs2nd2 §4: closed by `native-builtins/tests/oom_restartable_ratchet.rs` (only the 21 frozen bodies can be replayed, so none of the 13 writers can double-write). zs3nd3 §4: unit rows `zt6nd4_invoke_generic_dispatch_tests`. zs7nd6 §3 / zs9nd8 §4: host rows only (one `CRATONVM_DBG_RTERR=OutOfMemory` run of `Zs3Nd3UpcallMints mixed 4096 20000`; the `--compatible` `Zs5Nd5ProxyGrowth 6 500` 19/20 stderr). Report: `docs/internal/zgc-round-zt-20261018/t6-nd4-report.md`.**

> **STATUS (2026-10-19, zt t5/pg3): OPEN.** Item 2 DECIDED. Open: 1, `ClassLoader.addClass` after publish (`define_class_via_full`, native-builtins/src/classloader.rs:6911: presize `classes` before the reservation); 3, adoption-time allocations (park the overlay wrapper in `try_reserve_class_mirror_parking`); 4, the Spring builders' instance mints (`spring_startup_bootstrap`, `try_alloc_object` + `retry_after_reclaim`); 5, `DefinePhase::MirrorReserve` in classloading's define census. Carried here: zs2nd2 §4 (the 13 unread argument-array writers: read them or extend `zs2nd2_forced_refusal_replay_tests`), zs3nd3 §4 (a unit test driving `invoke_generic`), zs7nd6 §3 / zs9nd8 §4 (the upcall abort's door: one `CRATONVM_DBG_RTERR=OutOfMemory` run of `Zs3Nd3UpcallMints mixed 4096 20000`; the `--compatible` `Zs5Nd5ProxyGrowth` 19/20 stderr).

> **STATUS (2026-10-19, zt t3/nd): unchanged (items 1, 3, 4, 5 open as the t2 STATUS says). Report: `docs/internal/zgc-round-zt-20261018/t3-nd-report.md`.**

> **STATUS (2026-10-19, zt t2/nd): item 2 DECIDED by the orchestrator (G1/Gen keep dev's one-attempt doors: `ffm_mint_collects` and `defined_class_mirror` gated on `heap_is_zgc`, 4d9001deab). Items 1, 3, 4, 5 still OPEN, sharpened in the t2 report §3: item 1 needs the presize under `classes`' monitor and CratonVM's monitor identity across a moving collection is not established by reading (`NativeThreadAccess::monitor_enter`'s doc: the caller must not hold the object across a GC), so it was not built; item 3 is off the default paths; item 4 is the Spring bootstrap layer (CratonVM-only, `--compatible`); item 5 needs a `DefinePhase` in the classloading crate. Report: `docs/internal/zgc-round-zt-20261018/t2-nd-report.md`.**

> **STATUS (2026-10-19, zt t1/nd): OPEN, found by reading the round head `2dc18ab9a0` (zs + dev merge `d05124b2e3`).** Lane report: `docs/internal/zgc-round-zt-20261018/t1-nd-report.md`. What the lane fixed is listed there; this page is what it left, each with what would close it.

## 1. `ClassLoader.addClass` runs after the class is published (`--compatible` definer)

`classloader::define_class_via_full` (native-builtins/src/classloader.rs, the `if !hidden`
block after `defined_class_mirror_reserved`) calls `record_with_loader` -> `loader.addClass(mirror)`
AFTER `define_class_full` published the class and after the mirror was adopted. HotSpot's
`SystemDictionary::define_instance_class` calls `ClassLoader.addClass` BEFORE it updates the
dictionary ("this can fail with an OutOfMemoryError (if it does, we will *not* put this class in
the dictionary)"). So an `addClass` refusal (its `ArrayList` growth) on a spent heap leaves the
class defined on CratonVM, and the program's retry meets `LinkageError: duplicate class
definition`; on HotSpot the retry defines it. The reserved mirror (zs s7-s9) does not cover it:
the refusal is a Java allocation after the define, not the mirror.

Reached by: `define_class_via_full`'s callers (`classloader.rs` `cl_define_class0/1/2`-style
bodies, `shared_secrets_bridge` `JavaLangAccess.defineClass`); `lang_system`'s `defineClass1/2`
(the `--jdk-only` path) do not call `addClass` at all.

**What would close it:** grow the loader's `classes` list before the reservation
(`ensureCapacity(size() + 1)` through `invoke_virtual`, under the same pins), so the only
allocation `addClass` makes happens before anything is published; or reserve the list slot the
way the mirror is reserved. Deciding row: `Zt1NdDefineRetry` under `--compatible` with a loader
whose `classes` list is at a growth boundary (10, 15, 22, ... entries) -- `linkage=0` on HotSpot.

## 2. The merge made zs's "ZGC only" native-door gates every-collector

> **DECIDED (2026-10-19, orchestrator): gate on ZGC.** G1 and Generational are out of the zt round's scope and keep dev's one-attempt doors: `ffm_mint_collects` and `defined_class_mirror` now ask `ctx.heap_is_zgc()` first. Item 2 is closed in code (pending the build and the `Zs2Nd2FfmMints` ZGC rows).

zs's FFM and definer conversions gate on `NativeContext::native_doors_collect_on_refusal()`,
which was ZGC-only when they were written. Dev's gci i48/zl2 made it true on Generational and G1
(`vm/src/vm/vm_exec.rs` `native_doors_collect_on_refusal`). After the merge these collect on
every collector:

- `panama::ffm_mint_collects` users: the general downcall `ADDRESS` return
  (`panama.rs`, `pe_address_read_segment_with(ctx, rl, address, collecting)`; one-attempt on dev),
  the upcall address argument (`panama_upcall.rs` `new_arg_segment`), and through
  `upcall_args_collect` the by-value group arena and the generic invoker's mints;
- `lang_system::defined_class_mirror` (every definer's mirror after the define; dev's
  i22/oo `try_get_class_mirror` declines on G1/Gen too, so the ladder engages there).

`reserve_defined_class_mirror` is a no-op off ZGC (`try_reserve_class_mirror` answers
`Unsupported`). Behaviour on G1/Gen is now closer to HotSpot (collect before the error), but it
is untested there and the round's rule keeps G1/Gen behaviour unchanged. zt t1/nd corrected the
doc comments only.

**What would close it:** a maintainer call. Either keep (and add G1/Gen rows: `Zs2Nd2FfmMints`
`mixed` / `define` with `-XX:+UseG1GC` and `-XX:+UseSerialGC`), or gate these sites on
`ctx.heap_is_zgc()` (new in zt t1/nd) to restore dev's one-attempt doors there:

```diff
 pub(crate) fn ffm_mint_collects(ctx: &dyn NativeContext) -> bool {
-    ctx.native_doors_collect_on_refusal()
+    ctx.heap_is_zgc()
+        && ctx.native_doors_collect_on_refusal()
         && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_FFM_MINT_COLLECT")
 }
```

(the memo site already decides by collector: `panama::memo_address_return_collects`).

## 3. Adoption-time allocations after the define

`vm_object::class_mirror_impl` with a reserved mirror still allocates after the define in two
cases: the slot-0 class-id overlay (`try_set_field` boxes a 24-byte wrapper; only with a synthetic
`java.lang.Class` or `CRATONVM_CLASS_MIRROR_SLOT0_OVERLAY=1`), and a name the reservation did not
park (a wrong `define_name_hint`, the opt-in pure-bytecode lever). A refusal there declines the
adoption; `defined_class_mirror` then collects (the reserved object is garbage by then, so the
retry normally fits). Only a heap that the collection cannot relieve leaves the class defined.
**What would close it:** park the overlay's wrapper in the reservation too (allocate it in
`try_reserve_class_mirror_parking` and store it at adoption). Low priority: neither case is on the
default `--jdk-only` or `--compatible` path with a real `java.lang.Class`.

## 4. Framework instance mints after a define

`spring_startup_bootstrap` lookup-override / replace-override builders (`ctx.new_object` after
`define_class_full`) keep the one-attempt door (zs3nd3 §1 second half). zt t1/nd converted the
`Proxy.newProxyInstance` half only (`CRATONVM_ZGC_PROXY_INSTANCE_COLLECT`).

## 5. The define census does not see the reservation

The reservation's allocation (`reserve_defined_class_mirror`, before `define_class_full`) falls
between `supertype-prepass` and the backend's phases, so `CRATONVM_DBG_DEFINE_CENSUS` charges it
to no phase, and `mirror-alloc` reads ~0 on an adopted mirror. **What would close it:** a
`DefinePhase::MirrorReserve` (classloading's `define_census`), opened around the reservation in
the definers.
