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

## Validation

Build `cvmcref.exe` (unique name). Oracle harness (checksums == HotSpot):
- bt10=135854, bt14=3222190, bt16=14985902, bt18=68332206 — flag **on** and
  **off**.
- `CRATONVM_GC_STRESS` on bt16.
- `CardTest` old→young card barrier under GC_STRESS (= 529637376).
- `CtorTest`, `CollSmall`/`CollTest`.
- Measure node size (56 vs 72) and bt throughput delta.

## Gotcha: synthetic objects that store the wrong type into a reference slot

The legacy 16-byte tagged cell can hold *any* `Value` regardless of the field's
declared type, so some VM bootstrap code repurposes a slot with a value whose
type differs from the class's declared field. Under compact, a slot's width +
ref-ness is fixed by the layout: writing a non-`Object` into a reference slot
**auto-boxes** it (1-field wrapper), turning a "should-be-null" reference into a
non-null wrapper Object.

The first instance found: `ensure_system_streams` stored the synthetic fd id
(`Int(1/2)`) into slot 0 of the System.out/err PrintStream, but slot 0 of the
real `java/io/PrintStream` is the reference `out`. Under compact the Int
auto-boxed → `out` non-null → `route_write_through_out` routed every write into
the dead wrapper → silent empty stdout (computation + file I/O were unaffected).
Fixed by skipping the fd-tag store when slot 0 is a compact reference field
(`stream_fd` uses pointer identity in real-JDK mode). **When soaking the app
gauntlet, watch for the same pattern** (a primitive stored into a declared
reference field of a synthetic/bootstrap object).

## Gotcha (FIXED): legacy instances of a compact-layout class + inline codegen

**Symptom:** a hard SIGSEGV (`EXCEPTION_ACCESS_VIOLATION`, `pc` in a non-`.text`
heap address — a jump through a corrupted pointer) ~1 s into a reflection /
class-load-heavy workload (found on the commons-math `transform` suite run via
the JUnit Launcher API). Flag-off is clean.

**Root cause:** compactness is a **per-object** property, decided at allocation
by `plan_object_alloc` as `layout.field_count() == num_fields` and recorded in
the header's `GC_FLAG_COMPACT` bit. A class can have a **registered compact
layout yet still allocate LEGACY (16-byte-cell) instances** whenever an
allocation's `num_fields` disagrees with the layout's field count — most
commonly native/synthetic-stub allocations whose *padded* stub field count
exceeds the real declared count the layout was built from (observed:
`java/lang/reflect/Method` alloc 23 vs layout 20, `ConcurrentHashMap` 16 vs 12,
`ArrayList` 4 vs 3, plus the stream/`Path`/`Collector` stubs with layout count
0). Every **heap** and **helper** field-access path already keys on the
per-object `GC_FLAG_COMPACT` bit and handles both layouts — but the **JIT inline
getfield / putfield-ref** emitters (the perf lever) baked the compact byte offset
and assumed *every* instance of a registered-layout class is compact. Reading a
legacy object at the compact offset returns a mangled word (the 16-byte cell's
`{tag=4, partial-pointer}` first qword read as an 8-byte pointer, e.g.
`0x1AB297C0_00000004`); when that bogus reference is later dereferenced/called
the VM jumps through it → SIGSEGV.

**Fix:** make the inline emitters key on the per-object flag, exactly like every
other access path. Inline getfield now tests `gc_flags & GC_FLAG_COMPACT` (header
byte 21) after the null check and branches: compact → 8-byte ref / packed cell
read; legacy → the uniform `index * SLOT_SIZE` 16-byte-cell read (mirroring the
non-compact inline arm), all inline (no helper). Inline putfield-ref adds the
same not-compact case to its bail set, routing legacy receivers to the
per-object-flag-aware `jit_putfield_object` helper. Primitive putfields already
routed through the type helpers (`putfield_int/long/…`), which honor the flag, so
they needed no change. (`jit/src/x64.rs`, getfield opcode `0xb4` compact arm +
putfield opcode `0xb5` compact-ref arm.)

Validated: commons-math `transform` compact-ON now runs with **no SIGSEGV** and
the same 54–56/56 pass rate as compact-OFF (both vary run-to-run identically);
bt10/14/16/18 checksums exact compact-ON incl. `GC_STRESS`; flag-off unchanged.
A gated diagnostic `CRATONVM_DBG_COMPACT_LEGACY=1` (`gen_heap.rs`
`plan_object_alloc`) prints every class that allocates a legacy instance despite
a registered layout — use it to spot the same pattern when soaking new apps.

### Second fix: old-gen walker sized promoted compact objects as legacy

Running the same suite under `CRATONVM_GC_STRESS=1` surfaced a *distinct*
compact SIGSEGV (this one at a native/JIT `.text` `pc`, not a heap jump).
`OldGen::scan_region` / `scan_region_filtered` (`old_gen.rs`) — the walkers
behind `walk_objects` (major-GC compaction) and `walk_objects_in_card_ranges`
(minor-GC dirty-card scan) — sized every `kind == Object` as
`HEADER_SIZE + num_slots*SLOT_SIZE`, **ignoring `GC_FLAG_COMPACT`**. A promoted
compact object (body in `array_length`, e.g. 152 B) was therefore strided as
`num_slots*16` (e.g. 200 B), which (a) tripped the size-consistency skip in
`scan_dirty_cards` (`gen_object_total_size` said 152, the walker said 200) so its
old→young reference slots were **not scanned** — a missed root — and (b) desynced
the whole old-gen walk after that object. Fixed by sizing the `Object` arm via
`cratonvm_types::object_body_size(header)` (honours the per-object flag; legacy
objects still size as `num_slots*SLOT_SIZE`) in both `scan_region*` functions.
Regression-clean: bt10/14/16 exact, bt16 `GC_STRESS` exact, normal transform
unchanged.

### Third fix: refuse a compact layout built on padding (untyped containers)

`build_compact_layout` (`classloading/src/class.rs`) previously guessed every
**padded** slot (a `num_total_fields` slot with no field descriptor) as an 8-byte
reference. But the pure-padding classes are the untyped `ClassId(0)`-minted
synthetic containers — `cratonvm/synthetic/AnonymousObject$N`, i.e. **every
HashMap / LinkedHashMap node**, view backing, etc. — and native code stores MIXED
types into their raw slots (`map_alloc_node` writes `Int(hash)` into slot 0 and
object refs into slots 1-3). Guessing slot 0 is a reference makes compact
**auto-box** the int and gives the GC a wrong oop-map; under `GC_STRESS` this
strands the node's real reference slots. A class with no descriptors cannot have
a trustworthy oop-map, so `build_compact_layout` now returns `None` (→ legacy
uniform 16-byte tagged-`Value` cells, where every slot self-describes via its tag
— exactly the correct, compact-OFF behaviour for these containers) whenever any
padding is needed. Classes whose every slot has a real descriptor (bt `TreeNode`,
etc.) are unaffected and stay compact. Regression-clean: bt10/14/16 + bt16
`GC_STRESS` checksums exact, normal `transform` unchanged. Trade-off: HashMap-node
footprint reverts to legacy (they were never safely compactable without real
field types); the bt16 throughput win rests on `TreeNode`, which is unaffected.

### Residual (out of scope): GC_STRESS corruption is the moving-young GC-precision family

With all three fixes, compact-ON `transform` still shows a *caught, mostly
non-crashing* corruption under `GC_STRESS` only (`java/util/logging/Level`
init → `ClassCastException` / linkage error). Minimal deterministic repro:
`scratch/GcStressRepro.java` (Node churn **plus** `Level.INFO` init); the
**identical repro with the logging removed is 100 % clean**, so the corruption is
specific to the **weak-reference / reference-processing path**, not general
compact GC.

Root cause established (2026-07-01), **not a compact-layout bug and not a single
fixable defect**:
- Byte-level heap scans show an 8-byte pointer written at a **legacy
  `java.lang.ref.Reference`'s offset 0** (its `class_id`); the corrupt values
  resolve to `Level$KnownLevel` and `ReferenceQueue` — the weak-ref machinery.
- `interpreter.rs` reference processing enqueues/clears through **raw pre-GC
  addresses**; its stale-address guards (`is_stale_young` is young-only, the
  `num_fields<2` reused-slot check is admittedly weak — the code cites prior
  `bc-math-ec 0x4` and `h2-testscript-segv` root-cause battles) miss an old-gen
  Reference whose slot was reused, linking garbage into a live `ReferenceQueue`.
- BUT `CRATONVM_DBG_NO_REFPROC=1` (disable ALL reference processing) does **not**
  make it clean — it still corrupts occasionally and yields a *wrong* `sum` — so
  reference processing is one manifestation, not the sole writer. The corruption
  is **nondeterministic and multi-faceted**: the deep moving-young /
  missed-root GC-precision family that is an active open cluster **even without
  compact** (see the GC/moving-young/roots memory group). Compact's smaller
  footprint merely shifts GC timing enough to expose it on this load.

Because it is compact-*exposed*, not compact-*caused*, and disabling the prime
suspect subsystem does not resolve it, a targeted compact-side fix cannot close
it; it belongs to the ongoing GC-precision effort. Flag stays default-OFF, so
this torture-mode residual gates nothing.

Two lesser threads noted along the way (edge/footprint, gated behind the
default-OFF flag): (1) the JIT inline getfield ref path returns the bare slot
pointer and does **not** unbox an `AUTOBOX_CLASS_ID` wrapper the way heap
`get_field` does, so a primitive type-punned into a compact reference slot could
leak the wrapper to Java; (2) `set_field`'s compact autobox arm computes `base`
before `alloc_object`, safe today only because the heap `alloc_object` entry never
GCs — revisit if that changes.

**Deeper follow-up (optional, not required for correctness):** the legacy
instances themselves are a footprint miss, not a bug — they come from native/stub
allocations passing the padded stub count instead of the class's real
`num_total_fields`. Reconciling the synthetic-stub field counts with the real
declared layout would let these classes go compact too (more footprint win), but
is a larger, riskier change; the per-object flag already makes both layouts
correct, so this is left as a throughput lever, not a correctness fix.

## Stage 5 — observability (implemented)

HPROF instance dump (`serviceability.rs`) and the field-watch corruption
detector (`ec_watch.rs`) still assume the uniform 16-byte cell layout under the
compact flag are now compact-layout-aware:

- `serviceability.rs` resolves per-class packed offsets from `class_layout` and
  reads compact object references as raw pointers at offset `0` in compact
  reference fields.
- `ec_watch.rs` resolves packed field offsets and compacts-flag-aware watched
  slot kinds; compact reference watches read the watched 8-byte pointer payload.

Both features remain debug-only/off by default and do not affect execution,
GC, or benchmark checksums.

## Residuals / future

- The **opt-in IR JIT backend** (`CRATONVM_JIT_IR_*`) bails to single-pass for
  getfield/putfield/new under the compact flag (it bakes
  `HEADER+index*SLOT_SIZE` displacements). Native compact codegen for the IR
  tier is future work.
- The JIT field/alloc path under compact uses the **helper** path (no inline
  field store / inline-TLAB), so it is correct but not yet as fast as a
  compact-aware inline emitter — the next perf lever once correctness is soaked.
- Debug-flag-gated GC diagnostics (`rset-audit`, `small4`, `seedhunt`) still
  tag-scan compact objects (wrong logging only).
- Slice (2): compact **primitive** fields to natural width (needs descriptor at
  every read — bigger).
- Slice (3): full HotSpot-style packed header + typed fields.
