> **MOVED (20261019, zt round orchestrator):** out of the ZGC set: its open part belongs to another owner (zt t5/pg3 audit, `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md`). Former name `docs/known-issues/footprint/remaining-per-class-metadata-owners-20261014.md`.

# What a loaded class still costs after zs s2/fp2 (12.7 KB), by owner

> **STATUS (2026-10-19, zt t5/pg3): OTHER-OWNER (VM footprint / class metadata).** The census owners (constant pool slots, nested attributes, method records, dispatch snapshots) are classloading and VM code. Move to `docs/known-issues/footprint/remaining-per-class-metadata-owners-20261014.md`.

> **STATUS (2026-10-19, zt t3/fp): partly; one owner fixed, one nit.** The padded-code memo no longer clears whole and is counted (`[vm] padded-code-memo:`); InnerClasses decode interns the empty name once per attribute, not per entry (zs3rv2 §9). The rest as in the t2 STATUS. Rows: `docs/internal/zgc-round-zt-20261018/t3-fp-report.md`.

> **STATUS (2026-10-19, zt t2/fp): the largest VM-side owner has an opt-in lever, PENDING BUILD/PROBE; the rest OPEN.** The dispatch snapshots (~1.9 KB a class: `CachedBytecodeMethod` + padded code) are built on first dispatch under `CRATONVM_VTABLE_LAZY_DISPATCH=1` (default OFF; `VtableManager::entry_dispatch` / `defer_dispatch`, vm/src/runtime/vtable.rs:622 / :668; census part `lazy_cells`). Not built, by bytes: the packed constant pool (-0.4 KB, 173 `Utf8` sites), raw nested `Code` attributes (891 B), thin `method_records` names (-0.1 KB). Refuted as not worth a blind change this wave: thin names in `VtableSlotData` (classloading/src/class_manager.rs:5129/5133; -16 B a declared slot, ~0.1 KB a class, and every VM consumer converts to `Arc<str>` -- `VtableEntry`, `CachedMethodParts` -- so it saves nothing until jit-api goes thin too). New: the padded-code memo is an uncounted owner (`zgc-zt2fp-padded-code-memo-is-filled-by-the-link-and-clears-at-16384-FIXED-20261019.md`). Rows: `docs/internal/zgc-round-zt-20261018/t2-fp-report.md` and `t2fpab.spec`.

> **STATUS (2026-10-15, zs s6/rs): boot share, per VM rather than per class.** Not a per-class cut: s6/rs removed boot-time tables that are sized to the JDK or to a class-id bound and were written in full -- the jimage per-class index (~5 MB, `CRATONVM_JIMAGE_PACKAGE_INDEX`) and nine fixed-capacity side tables (~9 MB of writes, `CRATONVM_LAZY_ZERO_TABLES`). The per-class owners in the table below are unchanged. The new `CRATONVM_DBG_ALLOC_CENSUS` line gives the live requested bytes by size band at exit, which a `classes 0` vs `classes 3000` pair turns into a per-class allocator cost on the host. Rows: `docs/internal/zgc-round-zs-20261014/s6-rs-report.md`.

> **STATUS 2026-10-15 (zs s5/pg): PARTLY -- s3/fp3 cut three owners in code: `CRATONVM_TYPE_MAP_DROP_ROWS` (ON, 72be304cbb), a boxed `enclosing_method`, and the VM vtables. The loader census went 12633 -> 11294 B/class; not host-measured. Still open: dispatch snapshots (1.9 KB), the packed constant pool, `code_nested_attr_slots`, thin names in `method_records`, and the other listed owners.**

> **STATUS (2026-10-14, zs s3/fp3): partly FIXED IN CODE, PENDING HOST PROBE; the rest OPEN.** Local loader census (same setup): **12 633 -> 11 294 bytes a class**. The verifier rows are dropped by class stores (`store_type_maps` 1 979 -> 708; `CRATONVM_TYPE_MAP_DROP_ROWS=0` kills); `Class::enclosing_method` is boxed (`class_structs` 632 -> 568). The VM's own vtables are counted now (`vm/tests/zs3_fp3_vm_vtable_census.rs`) and cut **4 979 -> 3 235 bytes a class**; loader + VM side together 17 612 -> 14 529. Corrections to the table below: `Option<CodeSource>` is NOT rare: every defined class carries one (`class_manager.rs` `define_class_with_options`, the `code_source` match: a class-path / jmod URL or the synthetic `file:/runtime-defined/<name>.class`), so boxing it costs +79 bytes a class (measured, reverted). Still open, largest first: the dispatch snapshots (1.9 KB, proposal `zgc-zs3fp3-proposal-dispatch-snapshots-on-first-dispatch-20261014.md`), the constant pool (1 685, packed -0.4 KB, not built: 173 `Utf8` sites), `code_nested_attr_slots` 891, `method_records` 729 (thin names -0.1 KB, not built), `vtable_snapshots` 664, the type-map slots + signatures 682, `class_file_buffers_pinned` 563, `member_attr_slots` 546. Rows: `docs/internal/zgc-round-zs-20261014/s3-fp3-report.md`.

> **STATUS (2026-10-14, zs s2/fp2): OPEN.** These numbers are from the local deep census
> (Windows, debug test build, JDK 25.0.3, 1 500 java.base classes,
> `zr11_md_deep_per_class_census`). After this lane's cuts a class costs **12 657 bytes**. It was
> 15 450 bytes after s1/fp. This page lists what is left, largest first, and what would cut each
> part. None of it is fixed. Host rows: `docs/internal/zgc-round-zs-20261014/s2-fp2-report.md`.

Area: footprint, class metadata (boot RSS on the default collector). Parent page:
`zgc-zr8fm-class-metadata-per-class-cost-20261013.md`.

## The census after the lane

| part | bytes a class | what would cut it |
|---|---|---|
| verifier type maps (`store_type_maps`) | 1 979 | Most of it is row data, 1 300 bytes (244 rows a class). A row is recorded at every instruction start. Recording rows only at stop points (calls, allocations, polls, throws) needs every consumer's pc query audited first; the proposal `zgc-zs1fp-proposal-verifier-type-maps-on-first-use-20261014.md` is the other route. Slots are 364 bytes and signatures 318 (thin since this lane). |
| constant pool slots | 1 685 | 12 bytes a slot now; it was 24. A tag byte array plus 8-byte payloads would make a slot 9 bytes (-0.4 KB a class). That needs `ConstantPool::get` to return a view by value at 173 `Utf8` sites. |
| interned Utf8 strings (shared, unique estimate) | 1 345 | These are shared across classes already. HotSpot's symbol table holds the same strings. |
| `code_nested_attr_slots` | 891 | The LNT / LVT / LVTT / StackMapTable slots inside each `Code` attribute, 32 bytes each, ~28 a class. A `Code` attribute could keep its nested attributes raw (one boxed slice) and decode them on demand. |
| `method_records` | 729 | `ClassFileMethod` is 64 bytes: two `Arc<str>` names (32) and a `Vec` of attributes (24). Thin names (`ThinArcStr`) would make it 56; a boxed attribute slice would make it 48. Both need edits at every `method.name` reader. |
| vtable snapshots | 664 | One `VtableMethodSnapshot` a declared bytecode method. The VM copies it again into a `CachedBytecodeMethod`; see the next row. |
| the class struct | 632 | It was 680; the pool's header shrank to 24 bytes. `Option<CodeSource>` is 72 bytes inline, and nearly always `None` for boot classes. `enclosing_method` is three `String`s inline (72). Seven other `Option<String>`s are 24 bytes each. Boxing the two rare ones would save ~130 bytes a class. |
| `vtable_descriptors` | 575 | 8 bytes a slot plus one 64-byte block per declared virtual method, after this lane's sharing (it was 1 121). Thin names in `VtableSlotData` would make a block 56 bytes. |
| `class_file_buffers_pinned` | 563 | The class-file buffers that `Code` views pin. |
| `member_attr_slots` | 546 | As `code_nested_attr_slots`, for method and field attributes. |
| local variable tables | 367 | HotSpot keeps these too (`-g` classes). |
| everything else named | ~1 000 | Line tables 195, field records 241, class-level vectors 240, class-level strings 129, annotations 50. |
| side tables (`zs1fp-map`, without type maps) | 364 | `cm_name_definitions` is the largest at 115. |
| UNNAMED after the tables | 1 338 | Not attributed. |

## Not in this census: the VM's second copy of every vtable slot

`vm/src/runtime/vtable.rs`, `install_vtable_descriptors`, builds a `VtableEntry` per slot from
the loader's descriptors at the install hook. Each entry holds:

- two `Arc<str>` names;
- the declaring class id and the method index;
- `Option<Arc<CachedBytecodeMethod>>`.

By reading (not measured), that comes to about 2.4 KB for a java.base class with ~23 slots:

- **The entries.** A `VtableEntry` is 56 bytes (`vm/src/runtime/vtable.rs:187`), so the
  vector is ~1.3 KB. An inherited slot shares the `CachedBytecodeMethod`
  (`inherited_dispatch`), but every entry is a copy.
- **The signature index.** `install_vtable` (`vm/src/runtime/vtable.rs:699`) builds one per
  class: an `FxHashMap<u64, SlotBucket>` with 32-byte buckets, so ~1.1 KB at 32 buckets.

A `ClassManager` census does not reach either part.

What would close it is the third step of
`zgc-zr11md-vtable-descriptors-are-copied-per-class-CLOSED-20261019.md`: keep one copy. The proposal
`zgc-zs2fp2-proposal-one-vtable-copy-in-the-vm-20261014.md` describes it.

## Deciding rows

- The deep census before and after each cut:
  - `zr11md-load per_class_b`;
  - the `zr11md-part` line of the owner;
  - for the VM-side vtable, a `VtableManager` byte census (not built).
- Host RSS rows R1/R2 of the s2-fp2 report.
