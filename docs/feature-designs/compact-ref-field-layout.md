# Compact reference-field layout

**Status:** Shipped (default on; `CRATONVM_COMPACT_REF_FIELDS=0` opts out —
`false`/`off`/`no` also accepted).

## What it does today

A **reference instance field is a bare 8-byte pointer**, not a 16-byte tagged
`Value` cell. A two-reference node (`TreeNode {l, r}`) is
HEADER(40) + 2×8 = **56 B** instead of HEADER(40) + 2×16 = **72 B**, about 22%
smaller, and the saving compounds across every reference-heavy shape — linked
structures, `HashMap.Node`, AST nodes.

`compact_ref_fields_enabled()` in `types/src/field_layout.rs` returns `true`
when the variable is absent, and the answer is read once into a `OnceLock`, so
the layout is fixed for the life of the process. The layout is honoured on
every real path: class layout construction (`classloading/src/class.rs`), the
collector (`gc/src/gen_heap.rs`), the interpreter
(`vm/src/runtime/interpreter.rs`, `.../jit_bridge.rs`) and JIT codegen
(`jit/src/ir_lower.rs`, `jit/src/x64/objects.rs` —
`emit_inline_body_compact_ref_putfield` and the fresh-ctor variant —
`jit/src/x64/bytecode_walk.rs`, `jit/src/x64/inlining.rs`).

A companion lever in the same file is also default-on:
`pack_fields_by_width_enabled()` / `CRATONVM_PACK_FIELDS_BY_WIDTH=0`.

Reference **array** elements were always 8 bytes (`REF_ELEMENT_SIZE`,
`types/src/heap_types.rs`); this feature is about instance fields.

Setting the opt-out restores the legacy uniform 16-byte-cell layout, which is
what makes A/B runs possible.

## Design rationale

---

## The key enabling fact

In CratonVM the **field-index is the public contract**, not the byte offset:

- `Unsafe.objectFieldOffset` returns the **field index** (`slot_index`), not a
  byte offset (`native-builtins/src/lib.rs` `native_unsafe_object_field_offset1`).
- Reflection (`Field.get/set`), JNI field ids, VarHandle, deopt materialize, and
  the bytecode interpreter all call the **index-based API**
  `heap.get_field(obj, index)` / `set_field(obj, index, value)`.

So the index→byte-offset mapping is **purely internal to the heap**. We can
change the physical layout while keeping every index-based caller unchanged. The
blast radius collapses to the sites that compute a byte offset directly:

1. heap slot helpers `slot_ptr` / `read_slot` / `write_slot` (`gen_heap.rs`,
   `heap.rs`) — the central index→offset mapping.
2. the GC object-field scan/remap loops (which today detect references by the
   16-byte cell **tag**).
3. JIT inline getfield/putfield codegen (`jit/src/x64.rs`) + helpers
   (`vm/src/jit/helpers.rs`).
4. HPROF instance dump (`serviceability.rs`) and field-watch (`ec_watch.rs`).

## Layout

Per class, each instance field index `i` gets a **byte offset within the object
body** and an `is_ref` bit. Layout is a **prefix sum in declaration order**
(supers first, exactly as `compute_field_layout` already assigns indices):

```
offset(0)   = 0
offset(i+1) = offset(i) + size(i)          size(i) = 8 if ref else 16
body_size   = offset(last) + size(last)
field cell  = obj + HEADER_SIZE + offset(i)
```

- **Reference field:** 8 bytes, a bare pointer (`0` = null), identical encoding
  to reference **array** elements (which already use `REF_ELEMENT_SIZE = 8`).
- **Primitive field:** unchanged 16-byte tagged `Value` cell. Keeping primitives
  tagged means every *no-descriptor* reader (`get_field` without a type, generic
  copy, debug dumps) still decodes a primitive correctly from the tag — only
  reference reads change, and an 8-byte slot unambiguously decodes to
  `Value::Object`.

**Why declaration order / prefix sum (not "group refs first"):** field indices
are hierarchy-wide and stable (`absolute_index = declaring.first_field_index +
instance_idx`). `get_field(childObj, i)` for an inherited index `i` must hit the
**same** offset the parent uses. Prefix-sum in declaration order makes the
parent's body an exact **prefix** of the child's body, so inherited offsets match
automatically. Globally grouping references would break that prefix invariant.

**Alignment:** `align_of::<Value>() == 8` (largest payload is 8 bytes), and every
offset is a multiple of 8 (8/16 increments off the 8-aligned `HEADER_SIZE = 40`),
so both the 8-byte ref slots and the 16-byte primitive cells stay correctly
aligned — same as today (cells already sit at 40/56/72…, 8-aligned not 16).

## GC oop-map (the critical correctness surface)

Today the GC decides "is this slot a reference?" by reading the 16-byte cell as a
`Value` and matching `Value::Object(Some(_))`. Untagged 8-byte ref slots have no
tag, so the GC needs a **per-class oop-map**: the byte offsets of the reference
fields.

The gc crate already bridges to class metadata via a `fn`-pointer hook
(`ClassInfoHook` / `install_class_info_hook`, used by OOB diagnostics). We add a
parallel, **eager** registry (not a per-scan callback — the scan is hot):

- `gc::class_layout` — a dense `RwLock<Vec<Option<Arc<CompactLayout>>>>` indexed
  by `class_id`. `CompactLayout { ref_offsets: Vec<u32>, field_offsets: Vec<u32>,
  is_ref: Vec<bool>, body_size: u32 }`.
- Populated by the VM when a class layout is finalized (and on
  redefine / `recompute_subclass_layouts`), **only when the flag is on**.
- GC scan/remap loops read `ref_offsets` once per object and iterate 8-byte
  pointer slots — no tag, no per-field rebuild. Under STW (most collectors) the
  read lock is uncontended; concurrent mark only reads (layouts are immutable
  once a class is loaded).

**Object body size** (needed O(1) by the linear sweep/Cheney walk in
`gen_object_total_size`/`object_total_size`): stored in the header's
`array_length` u32, which is **unused for `kind == Object`** (arrays keep using
it for length). A `object_body_size(header)` helper centralizes this; flag-off
keeps `array_length == 0` for objects and the legacy `num_slots * SLOT_SIZE`
sizing. `num_slots` keeps its meaning (instance-field count) for bounds checks
and layout indexing.

## Gating

`CRATONVM_COMPACT_REF_FIELDS` is read **once at startup** into an immutable
global (`compact_ref_fields_enabled()`); the layout is fixed for the process so
objects are never read under a different layout than they were written. The
default is compact; `CRATONVM_COMPACT_REF_FIELDS=0` selects the legacy arm for
A/B runs. The registry is populated only when the compact layout is enabled.

## Migration checklist (by subsystem)

- **types** (`heap_types.rs`): `REF_FIELD_SIZE = 8`; `object_body_size` helper.
- **gc/class_layout.rs** (new): `CompactLayout`, registry, install hook.
- **gc/lib.rs**: re-export; `compact_ref_fields_enabled()` global.
- **classloading**: build `CompactLayout` from field descriptors (prefix sum,
  parent prefix) at layout finalize + subclass recompute; register via hook.
- **gen_heap.rs / heap.rs**: `slot_ptr`/`read_slot`/`write_slot`/`get_field`/
  `set_field`/volatile variants; alloc sizing + default init;
  `gen_object_total_size`/`object_total_size` via `object_body_size`.
- **gc scan/remap**: `gen_heap` minor (2513) / major mark (4866) / card scan
  (5656) / young fixup (4945); `gc.rs` Cheney (264/283); `old_gen` compaction
  (898); `concurrent_mark` (696); `g1` (3098). Iterate `ref_offsets`.
- **jit**: field resolver returns `(byte_offset, is_ref, type_tag)`; `x64.rs`
  getfield/putfield + inline-putfield (lever #2); `helpers.rs`
  `jit_getfield_*`/`jit_putfield_*`.
- **peripherals**: `serviceability.rs` HPROF instance dump (1426);
  `ec_watch.rs` slot addr (107).

## Collector scope

bt runs the default **generational** collector (non-moving sweep + selective
promotion; evac≈0 but card-barrier old→young must be correct under GC_STRESS).
Stages 2–4 cover `gen_heap` + `old_gen` + `concurrent_mark`. **G1 / ZGC /
semi-space `gc.rs`** are migrated or, if not yet, the flag refuses to enable with
those collectors selected (fail-loud, never silent corruption).
