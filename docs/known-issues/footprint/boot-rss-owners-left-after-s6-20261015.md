> **MOVED (20261019, zt round orchestrator):** out of the ZGC set: its open part belongs to another owner (zt t5/pg3 audit, `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md`). Former name `docs/known-issues/footprint/boot-rss-owners-left-after-s6-20261015.md`.

# Boot RSS owners left after zs s6/rs (Hello: ~210 MB on CratonVM, ~44 MB on HotSpot)

> **STATUS (2026-10-19, zt t5/pg3): OTHER-OWNER (VM footprint).** Items 1-4 are allocator / JIT-thread / native-registry / binary owners; item 5 (`Arena::rearm_alloc_anchors`) is shared by every backend. Move to `docs/known-issues/footprint/boot-rss-owners-left-after-s6-20261015.md`; the mimalloc levers' rows are Linux-only.

> **STATUS (2026-10-19, zt t3/fp): item 2's directory-record share FIXED IN CODE (default ON); items 1 and 4 still need rows.** `resource_to_modules` loses the ~2 300 `/packages` / `/modules` records and `module_names` the two pseudo-modules (`CRATONVM_JIMAGE_SKIP_DIRECTORY_RECORDS`). t2/rv FP-1..FP-3 applied to the first-touch / sparse-block hooks (populate at most 2 MiB of a commit without `huge`; a compile error on arches whose `MAP_*` values differ; `ARENA_BYTES` only after registration). Rows: `docs/internal/zgc-round-zt-20261018/t3-fp-report.md`.

> **STATUS (2026-10-19, zt t2/fp): items 1 and 4 reviewed and given rows; item 2 has a new sub-page; the rest OPEN.** Review of the two s9 levers found no unsoundness and fixed two edges: a compiler start now bumps a per-lane epoch so a standby worker an earlier, detached start left parked leaves instead of joining the new pool (`CompileLane::park_standby`, jit/src/tiered.rs:1332), and mode `boot` refunds the budget of a mapping the kernel refused (`refund_boot_budget`, vm-cli/src/sparse_block_alloc.rs:250). New opt-in `CRATONVM_TIER_C2_STANDBY_SCALE` (promotion at depth x live workers, the shape of HotSpot's dynamic compiler threads). Item 2's `resource_to_modules` directory records: `zgc-zt2fp-jimage-directory-records-are-indexed-as-resources-CLOSED-20261019.md`. Arena anchors (item 5) are a GC file (lane al's `arena.rs`): unchanged. Rows: `docs/internal/zgc-round-zt-20261018/t2-fp-report.md` and `t2fpab.spec`.

> **STATUS (2026-10-16, zs s9/rs2): items 1 and 4 have opt-in levers, PENDING HOST PROBE; the rest OPEN.** s6a host: s6's two keys hold SAME and stay ON, but their RSS effect on this THP=`always` host was ~2-3 MB (D vs K: rshello 213260 vs 216144 KB), below the ~7-9 MB predicted; `SZ` (`SPARSE_BLOCK_MMAP=1`) took -13..-15 MB at flat-or-lower CPU (rshello 0.44 vs 0.47 s, rscls 0.67 vs 0.68 s), and `C2T1` -7..-13 MB. Item 1 (per-thread arena pages of the compiler threads): `CRATONVM_TIER_C2_STANDBY` (opt-in). Item 4 (shadow stacks and other boot-time zeroed big blocks under THP): a new mode `CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP=boot` maps only the first `CRATONVM_MIMALLOC_SPARSE_BLOCK_BOOT_COUNT` (64) big zeroed blocks, so the zr xmt CPU risk of mode `1` on GC-heavy rows (prim +5 %: per-cycle zeroed scratch re-faulted through fresh mappings) does not apply after boot. Proposal to make one of them the default: `zgc-zs9rs2-proposal-boot-big-blocks-off-the-thp-arena-by-default-20261016.md`. Rows: `docs/internal/zgc-round-zs-20261014/s9-rs2-report.md`.

> **STATUS (2026-10-15, zs s6/rs): OPEN.** s6/rs cut two owners in code (the jimage per-class index, ~5 MB; the
> fixed-capacity side tables written in full, up to ~9 MB of writes) and added the instruments that attribute the
> rest on the host (`CRATONVM_DBG_RSS_BY_MAPPING`, `CRATONVM_DBG_ALLOC_CENSUS`). This page lists what is left,
> largest first, with what would close each part. Every byte figure here is by reading or from earlier host rows
> (named); nothing below was measured by s6/rs. Rows: `docs/internal/zgc-round-zs-20261014/s6-rs-report.md`.

Parent pages: `boot-rss-by-owner-20261013.md`, `boot-footprint-owners-20261013.md`,
`remaining-per-class-metadata-owners-20261014.md`.

## The split the host has measured so far

From zr z7b / z8c and zs s1a / ab-s1bf (THP `madvise`, `lib/modules`, no `jmods/`):

| owner | eager arena (default) | lazy arena (`CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT=1`) |
|---|---|---|
| total maxrss | 206-219 MB | 128-148 MB |
| first mimalloc arena (`[anon:mimalloc]`) | 141 MB, all `AnonHugePages` | ~65-70 MB in 4 KiB pages (by difference) |
| binary text + rodata | 37 + 10.7 MB | same |
| `lib/modules` pages | 9.9 MB | same |
| Java heap + ZGC bitmaps | ~1 + ~4 MB | same |

So the gap to HotSpot is, in order: the arena's huge-page surplus (~70 MB), the allocator's own 4 KiB footprint
(~65 MB, of which per-class metadata is ~14.5 KB x ~900 classes, about 13 MB), and the binary (~48 MB of file
pages against `libjvm.so`'s much smaller resident share).

## What is left, largest first

1. **The arena's huge-page surplus, ~70 MB.** Lazy commit and lazy first touch both remove it and both failed the
   CPU bar (+7..23 %: the 4 KiB first-touch faults). zr z10/mt showed that blocks of 1 MiB or more are only ~18 MB at
   peak, so the surplus is mimalloc's partially used 64 KiB and 512 KiB pages, one set per thread per size class.
   - The s6 census line (`[cratonvm] alloc-census: live_kb= peak_kb= live_le64k_kb= ...`) against the
     `alloc:mimalloc` row of `rss-class` now says how much of the arena is live requested bytes and how much is
     page-level sparsity, and in which size band.
   - A suspect, not measured: the JIT's compiler threads (1 C1 + floor(log2(cpus)) C2, `jit/src/tiered.rs:280-297`)
     each allocate IR in many size classes, so each owns partly used small and medium pages. zr z7b's `--nojit` row
     held 112 MB of huge pages against 141 MB. The s6 extras row `C2T1` (`CRATONVM_TIER_C2_THREADS=1`) separates
     the per-thread share. Proposal: `zgc-zs6rs-proposal-compiler-threads-share-one-allocator-heap-20261015.md`.
2. **The allocator's 4 KiB footprint, ~65 MB.** Per-class metadata (fp, fp2, fp3 pages) is about a fifth of it. The
   rest is unnamed until the census rows come back. Named owners by reading, not yet cut:
   - the native registry: ~12 000 slots, three `Box<str>` copies of `&'static str` names per registration
     (`native-api/src/registry.rs:11992`, `registrations`), 8 invocation shards x 8 bytes a slot (`:11888`), two
     hash indexes: ~3-4.5 MB. Keeping `&'static str` names (the registrars pass literals) would drop ~1.5-2 MB and
     ~36 000 boot allocations; it needs a `register` signature that tells a literal from a computed name;
   - `resource_to_modules` (`classloading/src/class_path.rs`, the jimage arm): ~3 200 entries, a `Vec<String>` each,
     ~0.65 MB, including ~2 300 directory records of the image's `/packages` and `/modules` trees that are not
     resources (they also add the pseudo-modules `packages` and `modules` to `module_names`, so
     `scan_module_infos` probes `/packages/module-info.class`). A per-module-number list and skipping the two
     pseudo-modules would cut it; the second half changes `list_jimage_modules`' answer, so it needs its own row;
   - the module registry's per-package strings (`classloading/src/module.rs:378` `package_to_module`, ~1 000
     packages with a cloned module `String` each, ~140 KB) duplicates the jimage package index s6 added; one could
     serve both.
3. **The binary's file pages, ~48 MB.** Text pages touched once at boot: ~14 000 native registrations
   (`.register(` call sites) run straight-line code and read their literals from rodata once. Fault-around maps
   16 pages per fault on a cached file. Levers: a table-driven registration (data, not code), or link-order
   grouping of boot code. Owner: the build / native registry, not this round.
4. **Thread shadow stacks under huge pages, up to 2 MiB per thread that runs compiled code.** `gc/src/shadow_stack.rs:621`
   allocates `vec![0usize; 256 K]` (2 MiB, `alloc_zeroed`). In the eager arena, any written word makes its huge page
   resident. `CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP=1` (opt-in, zr z10/mt) puts every zeroed block of 1 MiB or more on
   its own 4 KiB mapping: z10's xmt rows measured -14..-15 MB on boot and cls at equal CPU (0.35 vs 0.37 s boot). With
   s6's zeroed side tables (4 MiB + 2 MiB) now going through `alloc_zeroed` too, that arm should save more; the s6
   A/B carries it as arm `SZ`. It is shared by every collector, which is why zr kept it opt-in; the orchestrator
   decides.
5. **`Arena::rearm_alloc_anchors`** (`gc/src/arena.rs:1688`): `vec![usize::MAX; (cap >> shift) + 1]`, 256-512 KiB
   written at heap construction. Non-zero fill, so it cannot be allocated zeroed as is; storing `value + 1` would
   make 0 the empty value. Shared by every backend: a GC-owner change.

## Deciding rows

`docs/internal/zgc-round-zs-20261014/s6rsab.spec` (interleaved CPU + maxrss) and `s6rsextra.sh` (one run per arm
with the RSS-by-mapping report and the allocator census, both modes, HotSpot twin).
