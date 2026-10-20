# Define paths and class names after zt t7/cl: residuals

> **STATUS (2026-10-19, zt t7/cl): OPEN, filed by reading branch `zt7-cl` (base `4088dc829e`).** What
> t7/cl fixed is in `docs/internal/zgc-round-zt-20261018/t7-cl-report.md`: the real-JDK define doors
> now call `ClassLoader.addClass` (`CRATONVM_DEFINE_ADD_CLASS`), and on ZGC a loader-defined class's
> mirror name stays out of the strong string pool (`CRATONVM_ZGC_CLASS_NAME_UNPOOLED`). This page is
> what those fixes leave, each item with what would close it.

## 1. Another thread's `addClass` can take the presized slot

Carried from zt1nd item 1 (t6/nd4 report finding 2). The ZGC presize
(`classloader::presize_for_add_class` / `presize_loader_classes`) grows the loader's `classes` list
under its monitor and lets go. A parallel-capable loader can then run `addClass` on another thread
and take the free slot. The define's own `addClass`, after the publish, grows the list again. That
growth is an ordinary Java allocation, so it collects before it throws (not re-read this session).
So the residual needs both a race in the window and a heap that the full collection cannot relieve.
In that case HotSpot leaves the class undefined; CratonVM leaves it defined, and a retry meets
`LinkageError: duplicate class definition`.

Considered and not built:

- **Hold the list's monitor from the presize to the upcall.** `define_class_full` can run the
  loader's Java code (load-time transformers, a supertype the pre-pass missed). A second thread
  blocked in `addClass` while holding what that code needs (a parallel-capable loader's
  per-name lock) is a deadlock HotSpot cannot have.
- **Reserve the slot.** Keep the `add(null)` placeholder and fill it after the publish. Every
  reader of `classes` (a heap walk, a `privateLookupIn` getter) would see a transient `null`, which
  HotSpot's list never holds.

**What would close it:** a VM-side undo of the publish when the upcall fails (remove the class from
its loader's namespace before anything can resolve it). Deciding row: two threads defining into one
parallel-capable loader whose list is at a growth boundary, on a spent `-Xmx32m` ZGC heap, with
`linkage=` counted as `Zt6Nd4AddClassBoundary` counts it. Until a run shows `linkage>0` with
`CRATONVM_ZGC_ADDCLASS_PRESIZE` on, this stays filed, not built.

## 2. A malformed class on a spent heap: `OutOfMemoryError` where HotSpot throws `ClassFormatError`

The presize and the mirror reservation (zs s7) both run before the parse. HotSpot parses first,
then creates the mirror, then calls `addClass`. Reached only when the bytes are malformed and the
heap cannot hold the list growth or the mirror. **What would close it:** a header-and-constant-pool
pre-parse before the presize (the define census's `prepass` already parses for supertypes under
`CRATONVM_DEFINE_ONE_PARSE`; the presize could wait for that parse's verdict).

## 3. The built-in loaders' `classes` lists

HotSpot records every class the platform and application loaders define too. CratonVM records it
only for a class defined through one of the doors t7/cl converted, with a loader object whose
`classes` list exists:

- `Unsafe.defineClass0` takes a loader pin only for a user-defined loader or the platform loader
  (`unsafe_natives::native_unsafe_define_class`). An application-loader argument gets no
  `addClass`.
- `--compatible`'s `Lookup.defineClass` door (`lookup_define::lk_define_class_b_pinned`) adds only
  for a lookup class that has a defining-loader row (a user loader).
- Classes the VM loads from the class path without a Java `defineClass` never reach any door.

Gap row: `Zt7ClAddClass 12 app` (`app classes:` line) against HotSpot under both modes. **What
would close it:** the same upcall wherever the VM publishes a class into a built-in loader object's
namespace. That object must be the JDK-built loader (`builtin_loaders_adopted`), never a VM-made
object without a `classes` list.

## 4. `defineClass0` with `initialize` and no class data runs `<clinit>` before `addClass`

`lang_system::native_classloader_define_class0` hands `initialize` to `define_class_full` unless
class data is attached. So the class initializer runs inside the define, before the `addClass`
upcall. HotSpot calls `addClass` at the define and initializes afterwards. This is observable only
by an initializer that reads its own loader's `classes` list. **What would close it:** defer the
initialization to after the upcall for every `initialize` define, as `init_after_class_data`
already does when class data is attached.

## 5. Names that `Class.getName()` asked for stay strongly reachable

Residual 1 of `zgc-zs4nd4-unloaded-class-names-stay-in-the-strong-string-pool-20261015.md` is
fixed for the mirror's own name String. A class whose `getName()` was called still leaves two
things behind:

- **Its binary name in the pool.** The native interns its answer
  (`lang_class::native_class_get_name`), which HotSpot does too (`java_lang_Class::name` interns
  into the `StringTable`). HotSpot's table is weak; ZGC's pool is strong unless
  `CRATONVM_ZGC_WEAK_STRING_TABLE` is on. **Closes with** that switch's flip (its rows are on the
  zs4nd4 page).
- **The `getName()` memo's global root, on every collector.** `CRATONVM_CLASS_GET_NAME_MEMO`
  (default ON; `lang_class::class_get_name_memo_fill`) keeps, per class id, a global-root handle to
  a 2-element `Object[]` holding the mirror's name slot and the answer. Nothing removes a row when
  its class unloads. `ReflectDoorCaches::class_names` has no unload hook; it is per VM and lives as
  long as the VM. So both Strings stay strongly reachable after the class is gone. The class id is
  never reused, so the answers stay correct; only the memory is lost. **What would close it:**
  drop the row (and `remove_global_root` its handle) when the class unloads (the unload pass that
  retires a class id, `memory::gc::unload_dead_class_metadata`). Alternatively, key the row
  weakly on the mirror. Deciding row: `Zt7ClClassNames` with a `getName()` call added inside the
  define loop. With the memo on (default), `growth_bytes_per_define` grows. With
  `CRATONVM_CLASS_GET_NAME_MEMO=0` plus the weak table, it stays flat.

## 6. A `=0` of a sibling switch pools the parked name at the reservation

`try_reserve_class_mirror_parking` (vm/src/vm/vm_object.rs) pools the parked name when
`CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION=0`. The reservation does not know the defining
loader, so under that kill a loader-defined class's name is pooled even with
`CRATONVM_ZGC_CLASS_NAME_UNPOOLED` on. This happens only under a kill switch. **What would close
it:** pass the define's "a loader object defines this" bit into the reservation, and park
unpooled for it.
