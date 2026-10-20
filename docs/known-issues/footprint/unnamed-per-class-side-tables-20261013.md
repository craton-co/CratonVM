> **MOVED (20261019, zt round orchestrator):** out of the ZGC set: its open part belongs to another owner (zt t5/pg3 audit, `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md`). Former name `docs/known-issues/footprint/unnamed-per-class-side-tables-20261013.md`.

# ~4.8 KB a class that no walk names: side tables, hash maps, copied strings

> **STATUS (2026-10-19, zt t5/pg3): OTHER-OWNER (VM footprint / class metadata).** Nothing here is ZGC: the residue is class-metadata allocator bytes (steps 1 and 3 done, step 2 judged low value by t2/fp). Move to `docs/known-issues/footprint/unnamed-per-class-side-tables-20261013.md` (new folder for the non-GC footprint pages).

> **STATUS (2026-10-19, zt t3/fp): OPEN, unchanged in substance.** One more owner the census did not see now has a counter (the padded-code memo, `[vm] padded-code-memo: entries=`) and no longer clears whole. Step 2 stays low value (t2 STATUS). Rows: `docs/internal/zgc-round-zt-20261018/t3-fp-report.md`.

> **STATUS (2026-10-19, zt t2/fp): OPEN, step 2 judged low value.** Step 2 (one side record per class instead of separate `FxHashMap`s) is bounded by the census: all named side tables together are 364 bytes a class (zs s2/fp2 table, `zs1fp-map` lines), so merging saves at most their per-map overhead, well under 0.2 KB a class, against a refactor of five maps' readers in classloading/src/class_manager.rs. Not built. The larger unnamed residue (1 321 B a class after s3/fp3) is still not attributed; the padded-code memo (vm/src/runtime/frame.rs:126) is one owner no census counts (`zgc-zt2fp-padded-code-memo-is-filled-by-the-link-and-clears-at-16384-FIXED-20261019.md`).

> **STATUS (2026-10-16, zs s10/pg2): OPEN -- the s3/fp3 row drop (`CRATONVM_TYPE_MAP_DROP_ROWS`, default ON) is now host-measured: zs orch-notes ab-s3a cls CPU equal, max-RSS D 245 MB vs s2b 257 MB (fp3 -12 MB; the K arm's figure is not recorded). Step 2 is still not built: `class_type_annotations`, `class_bytes_base_digest`, `metaspace_charges`, `vtable_descriptors` and `init_states` are still separate `FxHashMap`s on `ClassManager` (classloading/src/class_manager.rs). Step 3 is only partly done (s2/fp2 InnerClasses names and source file).**

> **STATUS 2026-10-15 (zs s5/pg): PARTLY -- Step 1 is done: `ClassManager::footprint_census` (classloading/src/class_manager.rs:10837) and `Class::footprint_census` (class.rs:3210) name the maps, and the 4.8 KB were mostly verifier type maps (s1/fp c06d4a04c9, rows dropped by s3/fp3 `CRATONVM_TYPE_MAP_DROP_ROWS`, ON). Step 3 is partly done (s2/fp2 interned InnerClasses names and source file). Step 2 is not built: `class_bytes_base_digest`, `metaspace_charges`, `vtable_descriptors`, `class_type_annotations`, `init_states` are still separate FxHashMaps (class_manager.rs:4106-4543). The s1/s2 cuts are host-covered by the s2b fp2 cls A/B; the s3/fp3 row drop is not host-measured.**

> **STATUS (2026-10-14, zs s3/fp3): the type-map row share FIXED IN CODE, PENDING HOST PROBE.** The rows (1.3 KB a class of the old unnamed share) are dropped by class stores (`CRATONVM_TYPE_MAP_DROP_ROWS`, default ON). UNNAMED after the named tables is 1 321 bytes a class locally (1 314 on the base).

> **STATUS (2026-10-14, zs s2/fp2): step 3 (intern the strings) PARTLY FIXED IN CODE, PENDING HOST PROBE.** The InnerClasses names are now `ThinArcStr`s over the constant pool's interned strings, where they were three owned `String`s. Local census: `class_level_strings` 343 -> 129, `class_level_vecs` 415 -> 240 bytes a class. The vtable snapshots' source-file name is interned too, so it is the pool's own string. The UNNAMED residue after the tables is 1 338 bytes a class (it was 1 306); see `remaining-per-class-metadata-owners-20261014.md`.

> **STATUS (2026-10-14, zs s1/fp): ATTRIBUTED; the largest owner PARTLY FIXED IN CODE, PENDING HOST PROBE.** The per-class hash tables this page suspected hold only ~360 bytes a class. That is `ClassManager::footprint_census()` and `ClassStore::footprint_census()`, new; the deep census prints a `zs1fp-map` line per table. The ~4.8 KB were mostly the verifier type maps, `store_type_maps` 3 094 bytes a class. Their per-method layout is now one block with a 32-byte header (`c06d4a04c9`), which takes the class to 15 450 bytes from 16 439. The residue after the tables is 1 306 bytes a class. With `CRATONVM_DEFINE_BOOT_MAPS_AT_LINK=1` it is 160. Of this page's three steps, step 1 is done (the tables are named). Steps 2 and 3 (fold the maps; intern the strings) are worth ~0.2 and ~0.34 KB a class. Page: `zgc-zs1fp-verifier-type-maps-were-the-unnamed-per-class-share-20261014.md`.

> **STATUS (2026-10-13, zr z11/md): OPEN, measured, not attributed.** What is left of a
> java.base class's 16.4 KB once the deep census has named every container it can reach.

Area: footprint, class metadata. Parent page:
`zgc-zr8fm-class-metadata-per-class-cost-20261013.md`.

## Measured (local, Windows, debug, 1 500 classes; `zr11_md_deep_per_class_census`)

- `zr11md-part UNNAMED(delta-named)`: **~4 760 bytes per class**.
- `zr11md-size` (net live bytes by allocation size, during the load window), the lines the
  named parts do not explain:
  - power-of-two blocks over 4 KiB: ~3.1 KB a class together (`<=8192` 1 393, `<=16384` 966,
    `<=32768` 595, the 64 KiB-256 KiB buckets ~730). Hash tables grown by doubling: the
    `ClassManager` maps keyed per class (`loaded_classes`, `class_bytes_base_digest`,
    `metaspace_charges`, `vtable_descriptors`, `name_definitions`, `init_states`, ...), the
    `ClassStore` name / subclass / implementor indices, and the global intern pool's table.
  - 96-byte blocks: 27 198 of them, 16 002 of which are the boxed `CodeAttribute`s; the other
    ~11 200 are not attributed.
- `zr11md-part class_level_strings`: 343 bytes a class of `String` copies of names that are
  already interned (`InnerClassEntry`'s three names, `nest_members`, `permitted_subclasses`,
  `signature`, `source_file`, `nest_host`, `module_name` on `Class`).
- `zr11md-part interned_utf8_unique_est`: ~1 345 bytes a class of unique interned Utf8
  strings (an estimate: the `Arc<str>` block plus a map slot). HotSpot keeps its symbols too;
  not a defect by itself.

## What closing it would take

1. **Name the maps.** Give `ClassManager` a `footprint_census()` that returns each per-class
   map's `capacity() * entry size` (crate-private fields, so it has to live in the crate), and
   print it from the census. That turns this page's ~3 KB of power-of-two blocks into named
   rows.
2. **Fold per-class maps into one record.** Several maps are keyed by the same `ClassId`
   (`class_bytes_base_digest`, `metaspace_charges`, `vtable_descriptors`,
   `class_type_annotations`, `init_states`): one `Vec` indexed by class id with a small struct
   per class would replace five hash tables (each ~1.1-2x its entries in slack, plus control
   bytes).
3. **Intern the class-level strings.** Make the `Class` string fields `Arc<str>` clones of the
   constant pool's interned names (the consumers mostly read `&str`): ~340 bytes a class.

## Deciding row

The deep census's `UNNAMED` and `zr11md-size` lines before and after each step.
