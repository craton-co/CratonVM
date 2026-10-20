> **MOVED (20261019, zt round orchestrator):** out of the ZGC set: its open part belongs to another owner (zt t5/pg3 audit, `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md`). Former name `docs/known-issues/footprint/boot-rss-by-owner-20261013.md`.

# Boot RSS by owner: the GC holds ~2 % of a Hello world; on Linux two thirds were mimalloc's huge-page arena

> **STATUS (2026-10-19, zt t5/pg3): OTHER-OWNER (VM footprint).** Not a GC defect (the GC holds ~2 % of a Hello world); the open owners are mimalloc's eager huge-page arena, JIT one mapping per body and the binary's file pages. Move to `docs/known-issues/footprint/boot-rss-by-owner-20261013.md`; its THP / mimalloc levers are Linux-only rows.

> **STATUS (2026-10-19, zt t3/fp): OPEN; two small owners fixed in code (default ON), the t2 opt-in levers still await rows.** Fixed: the jimage directory records (~2 300 index entries, `CRATONVM_JIMAGE_SKIP_DIRECTORY_RECORDS`) and the padded-code memo's whole-table clear (`CRATONVM_PADDED_CODE_MEMO_KEEP_LIVE`). The rows now have a Windows form (PeakWorkingSetSize) beside the Linux max-RSS one; the mimalloc levers (`SPARSE_BLOCK_MMAP`, `FIRST_TOUCH`) are Linux-only and inert on Windows. Rows: `docs/internal/zgc-round-zt-20261018/t3-fp-report.md`.

> **STATUS (2026-10-19, zt t2/fp): OPEN; three opt-in levers await one host battery.** `t2fpab.spec` measures, against D and the t1 binary, every unmeasured boot lever in one interleaved run: `CRATONVM_TIER_C2_STANDBY` (+ `_SCALE`), `CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP=boot`, `CRATONVM_VTABLE_LAZY_DISPATCH` (new, this lane), `CRATONVM_MIMALLOC_FIRST_TOUCH=huge` (its CPU was never A/B'd) and `CRATONVM_DEFINE_BOOT_MAPS_AT_LINK`. Still open: the eager huge-page arena (the largest owner), JIT one mapping per body, the binary. Rows: `docs/internal/zgc-round-zt-20261018/t2-fp-report.md` and `t2fpab.spec`.

> **STATUS (2026-10-16, zs s10/pg2): OPEN -- s6/rs's two owners (`CRATONVM_JIMAGE_PACKAGE_INDEX` and `CRATONVM_LAZY_ZERO_TABLES`, both default ON) have run on the host. zs orch-notes s6a: the rs hello/cls default rows are SAME, CPU is within noise of s5a, and both keys stay ON. Two caveats: rsmtree (D 15.35 vs K 13.47 s, runs 11.7-17.5) is re-run on s7a, which is queued and not run; and orch-notes records no D-vs-K RSS figure for these two keys. The other RSS levers measured there: `MIMALLOC_SPARSE_BLOCK_MMAP=1` -13 to -15 MB and `TIER_C2_THREADS=1` -7 to -13 MB at flat CPU. They went to s9/rs2, which is not host-measured. Still open: the eager huge-page arena (`CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT` and `FIRST_TOUCH=lazy` stay opt-in) and the owners on `boot-rss-owners-left-after-s6-20261015.md`.**

> **STATUS (2026-10-15, zs s6/rs): two more owners FIXED IN CODE, PENDING HOST PROBE; the arena surplus still OPEN.** (1) The jimage class index is per package (`CRATONVM_JIMAGE_PACKAGE_INDEX`, ON, `=0` kill): 27 086 classes / 65 536 buckets of `String` pairs (~5 MB, every page written) became 876 packages (~50 KB). (2) Side tables sized to a class-id bound are allocated zeroed instead of written element by element (`CRATONVM_LAZY_ZERO_TABLES`, ON, `=0` kill): field_layout replace counters 4 MiB, statics index 2 MiB, and seven 128-256 KiB tables. Under the eager huge-page arena a zeroed block still costs every 2 MiB it touches, so expect ~-7..-9 MB on the default arm and more with `CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP=1` (arm `SZ`). New instruments: `CRATONVM_DBG_RSS_BY_MAPPING=1` (the launcher prints this page's mapping classes at exit, so `/usr/bin/time` rows carry their own split) and `CRATONVM_DBG_ALLOC_CENSUS=1` (live requested bytes by size band, which says how much of `alloc:mimalloc` is live data and how much page sparsity). Remaining owners: `boot-rss-owners-left-after-s6-20261015.md`. Rows: `docs/internal/zgc-round-zs-20261014/s6-rs-report.md`.

> **STATUS 2026-10-15 (zs s5/pg): PARTLY -- This umbrella page closes per owner. The largest owner, the eager huge-page arena, is still the default: `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT` is opt-in (z10/mt revert), and `CRATONVM_MIMALLOC_FIRST_TOUCH=lazy` failed the CPU bar in zs orch-notes ab-s1bf (hello +23 %, churn +8 %; RSS 219 vs 148 MB). The jrt copy (`CRATONVM_JIMAGE_DIRECT_MAP`, ON; zr z8c host RSS 297-303 vs 430-441 MB) and the GC bitmaps off THP (`CRATONVM_ZGC_SIDE_TABLE_NO_THP`, ON) are fixed; the jmod index (`CRATONVM_JMOD_LAZY_INDEX`, ON) is fixed in code only; class metadata was cut by fp/fp2/fp3 (fp2 host cls -11 MB; fp3 not host-measured). Still open: the ~40 MB unnamed allocator remainder (R7), JIT one mmap per body (`ExecutableBuffer::new` -> `platform::alloc_executable`), and the binary size. Line references were refreshed; the "IR thread 64 MiB" is a `#[cfg(test)]` helper (`on_big_stack`), not a production thread.**

> **STATUS (2026-10-14, zs s1/fp): the arena's first-touch trade has an opt-in design, PENDING HOST PROBE.** `CRATONVM_MIMALLOC_FIRST_TOUCH` (`lazy`, `huge`, `populate`, `=1` = huge+populate). The launcher maps mimalloc's first arena itself and registers it uncommitted with a commit callback (`mi_manage_memory`). Whole 2 MiB chunks of big commits go on huge pages, and the 4 KiB rest is pre-faulted with `MADV_POPULATE_WRITE`. The aim is lazy commit's RSS at eager's fault count. Rows F0-F3 in `docs/internal/zgc-round-zs-20261014/s1-fp-report.md`. Proposal: `zgc-zs1fp-proposal-mimalloc-first-touch-arena-20261014.md`.

> **STATUS (2026-10-13, zr z10/mt): lazy arena commit REVERTED TO OPT-IN; ZGC bitmaps moved off the huge-page arena, FIXED IN CODE, PENDING HOST PROBE.** The z10 interleaved rows (5 rounds, CPU s median) put lazy commit at 0.95 vs 0.79 eager on `Zr7RsBoot classes 3000` (+20 %) and 0.53 vs 0.51 on boot; `MIMALLOC_ALLOW_THP=0` on the eager arm cost the same, so both the CPU (~35k extra 4 KiB first-touch faults) and the ~90 MB RSS saving are the huge pages'. Over the 1.05x bar: `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT` is opt-in now (`=1`, `force`; the summary prints `lazy_arena_commit=off`). By reading, the GC-owned share of the 90 MB is small: at `-Xmx256m` the start and mark bitmaps are 4 MiB each (old-start 4 MiB more once used), at most ~12 MB of huge-page waste. They now sit on private `MADV_NOHUGEPAGE` mappings (`CRATONVM_ZGC_SIDE_TABLE_NO_THP`, default ON, `=0` kill, `side_maps=` on the spans line). The rest of the surplus is not named yet: the launcher's allocator now counts every block of at least 1 MiB in every mode (`[cratonvm] mimalloc-sparse-blocks: big_live_peak_kb=`), which bounds what a big-block lever can reach; if it is small, the surplus is mimalloc's own partially used 64 KiB / 512 KiB pages, which no per-block lever reaches. The opt-in `CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP=1|all` maps every big zeroed (or every big) block on its own for the A/B. Rows in `docs/internal/zgc-round-zr-20261012/z10-mt-report.md`.

> **STATUS (2026-10-13, zr z9/rv8): hook fallback FIXED IN CODE, PENDING HOST PROBE.** The pre-`main` hook's `late` arm forced `arena_eager_commit=0` over an explicit mimalloc setting spelled in a way its probe missed (mimalloc matches environment names case-insensitively and takes the legacy `eager_region_commit`; a non-zero option after `mi_option_set_default` can only be such a setting). It now reports `env` and leaves it; `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT=force` keeps the old forcing. Rows R2 in `docs/internal/zgc-round-zr-20261012/z9-rv8-report.md`.
>
> **STATUS (2026-10-13, zr z7/rs follow-up): Linux DECOMPOSED, largest owner FIXED IN CODE,
> PENDING HOST PROBE.**
>
> **The largest owner.** The host run (z7b, THP `madvise`, no `jmods/`) put the first mimalloc
> arena at 141 MB of a 208.8 MB Hello world, all of it `AnonHugePages`.
>
> **Why.** mimalloc's default `arena_eager_commit=2` commits the 1 GiB arena up front on Linux.
> Because the arena is committed, mimalloc itself `madvise(MADV_HUGEPAGE)`s it
> (`libmimalloc-sys-0.1.49/c_src/mimalloc/v3/src/prim/unix/prim.c:413-421`; `allow_large`
> survives only when `commit` is true, `os.c:299/342`; reached from `arena.c:355`).
>
> **The fix.** `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT` is default ON with a `=0` kill. A
> pre-`main` constructor in `vm-cli/src/mimalloc_tuning.rs` sets the default to 0. It sits in
> `.init_array.00100`, which runs after std's 99 and before mimalloc's own 101. An explicit
> `MIMALLOC_ARENA_EAGER_COMMIT` still wins.
>
> **Expected.** `/usr/bin/time %M` was measured at 233 → 155 MB with the env variable. The rows
> that decide it are under "Follow-up rows" below.

> **STATUS (2026-10-13, zr z8/fm): two owners FIXED IN CODE, PENDING HOST PROBE.** The JMOD share (`CRATONVM_JMOD_LAZY_INDEX`) and the jrt image copy (`CRATONVM_JIMAGE_DIRECT_MAP`), both default ON with `=0` kills: `boot-footprint-owners-20261013.md` (rows F0-F2). Correction to item 3 below: the local-header walk is in `ZipArchive::new` itself (`zip-2.4.2/src/read.rs:708-710, 1259`), not in the `by_index_raw` loop, so `file_names()` alone would not have helped. Per-class metadata: `zgc-zr8fm-class-metadata-per-class-cost-20261013.md`.

> **STATUS (2026-10-13, zr z7/rs): OPEN, decomposed. Not a GC defect.** Read from code and
> measured locally (Windows working set, page by page). Linux decomposition rows are below and
> pending on the host. New instruments: `tools/rss-by-mapping.sh` (smaps, pagemap, classes),
> `[GC] zgc-footprint-spans:` (`CRATONVM_GC_STATS=1`, printed at heap construction and at exit),
> and probe `tools/probes/Zr7RsBoot.java` (`hello | gc | list | classes`). GC-side opt-in:
> `CRATONVM_ZGC_ARENA_NO_THP=1`. It only matters on a THP=`always` host. Handoff item #5.

## Linux decomposition (host, z7b binary, `Zr7RsBoot hello 1500`, `-Xmx256m`; orchestrator run)

Host conditions: THP mode `[madvise]`. `$JAVA_HOME/jmods` has 0 files, so the JDK boots from
`lib/modules`. All figures are smaps Rss at the hold.

| owner (script class) | CVM jdk-only | HotSpot ZGC |
|---|---|---|
| **total** | **208.8 MB** | **43.7 MB** |
| first mimalloc arena, `20000000000 rw-p 1048576K [anon:mimalloc]` | **141 MB, `anon_huge_kb=141312` (all THP)** | (no mimalloc) |
| binary text / rodata | 37 MB / 10.7 MB | |
| `lib/modules` (jimage) | 9.9 MB | |
| GC: Java heap | 1.1 MB | |
| GC: start + mark bitmaps | 4 MB, inside the mimalloc arena (`anon:holds-gc-span`) | |

Other rows on the same host:

| row | total |
|---|---|
| `--compatible` | 204 MB |
| `--nojit` | 173 MB (112 MB of it huge) |
| `-Xmx8g` | 213 MB |
| `-XX:+UseG1GC` | 213 MB |
| `MIMALLOC_PURGE_DELAY=0` | 199.6 MB (huge 45 MB) |
| `CRATONVM_ZGC_ARENA_NO_THP=1` | identical to R1, as predicted for a `madvise` host |
| `CRATONVM_DISABLE_JAR_MMAP=1` | 344 MB (286 MB huge) |

Per-class row (R5, `--nojit`): CVM 184 → 264 MB (about 27 KB per class), HotSpot 34 → 48 MB
(4.8 KB per class).

`jrt:/` row (R8, `list`): CVM 520 MB, HotSpot 108 MB.

`/usr/bin/time %M` sweep on Hello:

| arm | maxrss |
|---|---|
| default | 233 MB |
| `MIMALLOC_ARENA_EAGER_COMMIT=0` | **155 MB** |
| `MIMALLOC_ARENA_RESERVE=65536` | 237 MB |
| `PURGE_DELAY=0` | 225 MB |
| `CRATONVM_HEAP_THP=0` | 236 MB |
| eager 0 + purge 0 | 158 MB |

`MIMALLOC_VERBOSE=1` printed `arena_eager_commit=2`, `arena_reserve=1048576 KiB`,
`allow_large_os_pages=0`, `purge_delay=1000`.

What the Linux run settles:

- **Linux and Windows differ by the huge-page arena.** The Windows allocator share was 81 MB in
  4 KiB pages. On Linux the same allocator footprint, faulted in 2 MiB pages, is 141 MB.
- **With lazy commit**, mimalloc commits by slice and never advises huge pages. The remaining
  ~155 MB is ordinary 4 KiB-page allocator footprint (class metadata at ~5.6x HotSpot per class,
  the JIT, the boot tables), plus the binary (~48 MB of text and rodata against `libjvm`'s much
  smaller resident share) and the image.
- **The jmod term is absent on this host.** It applies only to a JDK that ships `jmods/` (the
  Windows Microsoft build here), and it stays filed below for that case.
- **The GC is still ~5 MB.** Its bitmaps live inside the mimalloc arena (they are `alloc_zeroed`
  blocks), so they were huge-page backed too. That is the allocator's doing, not
  `reservation.rs`, which advises only its own mapping and only under `CRATONVM_HEAP_THP`.

Area: footprint. Handoff item #5 says a Hello world's maxRSS is ~207 MB on CratonVM and ~42 MB
on HotSpot 25 (Linux host, `zgc-zr3fp` z3b rows). Nobody had split that number by owner.

## Local decomposition (measured)

How it was measured:

- Box and binary: Windows 11, i7-8550U. The binary is the 2026-10-03 release, which predates
  the zr round. That is fine here, because the round changed only collection-time paths and
  this is a boot measurement.
- Run: `Zr7RsBoot hello 6000`, which loads 786 classes on the jdk-only default.
- Method: `QueryWorkingSet` reads every resident page. `VirtualQueryEx` then assigns each page
  to its region type and file, during the hold.
- HotSpot reference: 25.0.3 with `-XX:+UseZGC`.

All values are working-set MB.

| owner | CVM jdk-only | `--nojit` | `--compatible --nojit` | HotSpot ZGC |
|---|---|---|---|---|
| **total** | **151** | 140 | 126 | **40** |
| process allocator (one 1 GiB private reservation: Rust's heap) | **81.1** | 76.7 | 62.8 | 14.1 (incl. its class space) |
| JDK files mapped: 70 `jmods/*.jmod` (CVM) / `lib/modules` (HotSpot) | **~47** | ~47 | ~47 | 1.7 |
| the binary's image (`cratonvm.exe` / `jvm.dll`) | 17.5 | 12.2 | 12.4 | 7.4 |
| system DLLs | ~5 | ~5 | ~5 | ~6 |
| **Java heap (the ZGC arena, 256 MiB reservation)** | **1.6** | 1.6 | 1.0 | (inside the 14.1) |
| **GC side tables (the 4 MiB bitmap allocations, ...)** | **~0.9** | 0.2 | 0.1 | |
| anonymous executable (JIT code) | 0.7 | 0 | 0 | 0.8 |

Other local rows (peak working set over the run, MB):

| row | value |
|---|---|
| `-Xmx256m` vs `-Xmx8g` | 154 vs 154 (flat in `-Xmx`) |
| ZGC vs `-XX:+UseG1GC` | 154 vs 159 (not collector-specific) |
| `-version` (no VM created) | 9 |
| `CRATONVM_DISABLE_JAR_MMAP=1` | 221 (archives read into memory: positive control for the "files" share) |
| HotSpot ZGC `-Xshare:off` | 33 (CDS is not HotSpot's advantage) |
| classes loaded | CVM 786 (jdk-only) / 627 (`--compatible`), HotSpot 834 (`ClassLoadingMXBean`, read after the hold) |
| per-class cost, `classes 0` vs `classes 3000 <list>`, `--nojit` / `-Xint -Xshare:off` | CVM +60 MB per 3000 classes, about **20 KB per class**; HotSpot +14 MB, about **4.7 KB per class** |

## What that says, owner by owner

1. **The GC: about 2.5 MB of 151 (under 2 %).** That is the arena's touched pages (a boot holds
   ~1.2-1.6 MB of live Java objects) plus the touched bitmap pages. Everything sized to `-Xmx` is
   address space and costs nothing until written:
   - the arena reservation is `PROT_NONE` / `MEM_RESERVE` and commits by granule
     (`gc/src/reservation.rs` `platform_reserve`, `platform_commit`);
   - the object-start and mark bitmaps are `alloc_zeroed` blocks, demand-faulted
     (`gc/src/heap_bitmap.rs` `HeapBitmap::labelled`);
   - the startup commit does not touch pages (`gc/src/arena.rs` `commit_initial_zgc`);
   - TLAB chunks skip the zeroing of pristine memory (`CRATONVM_ZGC_PRISTINE_CHUNKS`, on);
   - the bounded, sparse bitmap clears store nothing into never-written words
     (`ZgcRealHeap::mark_clear_all`).

   The only eager touches at heap construction are tiny. One is the arena's anchor table,
   `vec![usize::MAX; ≤64 Ki]` = ≤512 KiB, in `Arena::rearm_alloc_anchors`
   (`gc/src/arena.rs:1688`). It is shared by every backend, so it is left alone. The others are
   the per-granule vectors of the uncommit policy (≤ 64 KiB at 8g). **There is no GC-owned boot
   share worth a fix on Linux.**

   On a THP=`always` host there may be one. The first touch of a 2 MiB-aligned run would fault a
   huge page, and the arena's anonymous mapping would then cost 2 MiB per touched granule. HotSpot
   is not exposed to this: its ZGC heap is a `memfd` mapping. `CRATONVM_ZGC_ARENA_NO_THP=1`
   (opt-in) advises the arena `MADV_NOHUGEPAGE`. The script prints the host's THP mode and
   `AnonHugePages` per class, and row R4 decides.
2. **The process allocator: ~81 MB vs HotSpot's ~14 MB, the largest owner.**
   - **Class metadata.** About 20 KB per loaded class against HotSpot's ~4.7 KB, measured with
     `classes` (load and link, no `<clinit>`). At 786 classes that is ~16 MB: a real 4x, but it
     covers only a fifth of the 81.
   - **`--jdk-only` versus `--compatible`.** About +18 MB. The module-system boot runs real JDK
     bytecode.
   - **The JIT.** About +4.4 MB of allocator plus +5.3 MB of binary pages, and +0.7 MB of code
     (`--nojit` arm). Every compiled body is its own `mmap`: at least one page and one VMA per
     method (`jit/src/exec_memory.rs` `ExecutableBuffer::new` → `platform::alloc_executable`).
   - **The JDK archive indexes.** Every jmod's names are held twice, as `all_entry_names:
     Vec<String>` plus `class_entry_index: FxHashSet<String>`
     (`classloading/src/class_path.rs:7856-7864`). For 70 jmods that is a few MB.
   - **The remainder, ~40 MB, is unattributed by reading.** It needs an allocation-site profile
     (row R7).
3. **The jmods: ~47 MB of file pages for a Hello world.** That is 12 MB in `java.base.jmod`, 8 MB
   in `java.desktop.jmod`, and 4 MB each in `java.xml` and `jdk.localedata`. On a JDK that ships
   `jmods/`, the boot classpath is all 70 of them (`vm/src/config.rs` `discover_boot_classpath`).
   The jmods are mapped (`read_archive_for_classpath`), and `load_jmod` indexes them with
   `ZipArchive::by_index_raw(i)` for every entry (`classloading/src/class_path.rs:7857-7862`). In
   zip 2.4.2, `by_index_raw` calls `find_content` → `find_data_start`, which reads each entry's
   **local header** (`zip-2.4.2/src/read.rs:1096-1105, 336-344, 362-370`). That faults in a page
   for every entry of every jmod, which is close to every page of the files. The comment at
   `class_path.rs:7852-7855` ("reads no payload bytes at all") is true of payload, but not of the
   local headers.
   - **Fix direction (classloading owner, not done here):** take the names from the central
     directory only (`ZipArchive::file_names()` / `name_for_index`). Resolve the data start
     lazily, on the first `by_name` of a class.
   - **Expected:** most of the 47 MB goes. The pages are clean and file-backed, so they are
     reclaimable, but they count in maxRSS and in the working set.
   - A JDK without `jmods/` boots from `lib/modules`, which is mapped. There the walk in
     `load_jimage` (`class_path.rs:7383-7479`) touches the location tables and the strings of all
     ~30 000 entries. It builds a `class_to_module` map of ~30 000 `String` pairs where HotSpot
     answers by perfect hash per request. That is a smaller share of the same kind. Its fix
     direction is a package-to-module map (about 1 000 packages).
4. **The binary: 12-17 MB of image pages vs `jvm.dll`'s 7.4 MB.** It would grow on Linux by the
   relocated `.data.rel.ro` of a PIE binary (private dirty, `exe:data` in the script). Owner: the
   build, not this round.
5. **Thread stacks: small.** CVM runs 12-13 threads where HotSpot runs 26-27. The stacks are
   reserved large but touched shallow: `main-vm` 128 MiB (`vm-cli/src/main.rs:11949`), compiler
   threads 16 MiB (`jit/src/tiered.rs:5972`), the IR thread 64 MiB (`jit/src/ir_lower.rs:101020`, a `#[cfg(test)]` helper, not a production thread).
   On a THP=`always` host a touched stack can cost a 2 MiB huge page each. JDK-8303215 is
   HotSpot's fix for that. The script's `anon:thread-stack` `anon_huge_kb` column shows it.

**Why Linux is ~207 MB and Windows ~151 MB.** This is not measured yet. The likely reasons:

- mimalloc is the global allocator on Linux only (`vm-cli/src/main.rs:20-22`), and it retains
  freed segments until its purge delay;
- the PIE relocations;
- THP, if the host is `always`;
- `VmHWM` counts the boot's transient peak where the working set above is a sample during the
  hold.

Rows R1-R3 split it.

## Not boot, but found on the way: a `jrt:/` mount reads the whole runtime image into the Java heap

`Zr7RsBoot classes` first listed java.base through `jrt:/`. On CratonVM that run peaked at
**418 MB** working set, against 62 MB on HotSpot.

The cause is `NativeImageBuffer.getNativeMap`. Its native reads the ~150 MiB `lib/modules` into a
Java `byte[]` and caches it behind a global root for the life of the VM (`native-io/src/lib.rs:7824-7945`,
`native_jimage_get_native_map`). HotSpot hands back its own mapping instead.

On ZGC that is a 150 MiB live huge array. Any program that touches `jrt:/` pays it, which includes
javac in-process, Spring's classpath scanning of the JDK, and `ModuleFinder.ofSystem()` users. It
also needs `-Xmx` above ~160 MB to work at all.

- **Fix direction (NIO owner):** return a direct `ByteBuffer` over the image mapping the class
  path already holds. The native's comment says CratonVM's `FileChannel.map` snapshot does not
  survive `BasicImageReader`'s typed reads, so that has to be fixed first.
- **Deciding row:** R8.

## Rows for the host (Linux; run the VM directly, not under a wrapper)

`P=tools/probes` (compiled classes), `NAMES=/tmp/zr7rs-names.txt`. Every row: three repeats, and
the median.

```
# R0 setup: the class list, made ON HOTSPOT (a jrt mount on CVM costs ~150 MB, see above)
java -cp $P Zr7RsBoot list > $NAMES; wc -l $NAMES          # ~7378
cat /sys/kernel/mm/transparent_hugepage/enabled; ls $JAVA_HOME/jmods | wc -l

# R1 the decomposition, both modes, and the HotSpot twin
CRATONVM_GC_STATS=1 tools/rss-by-mapping.sh -l cvm-jdkonly -- cratonvm -Xmx256m -cp $P Zr7RsBoot hello 1500
CRATONVM_GC_STATS=1 tools/rss-by-mapping.sh -l cvm-compat  -- cratonvm --compatible -Xmx256m -cp $P Zr7RsBoot hello 1500
tools/rss-by-mapping.sh -l hs-zgc -- java -XX:+UseZGC -Xmx256m -cp $P Zr7RsBoot hello 1500
tools/rss-by-mapping.sh -l hs-zgc-noshare -- java -XX:+UseZGC -Xshare:off -Xmx256m -cp $P Zr7RsBoot hello 1500
# R2 subsystem deltas
CRATONVM_GC_STATS=1 tools/rss-by-mapping.sh -l cvm-nojit -- cratonvm --nojit -Xmx256m -cp $P Zr7RsBoot hello 1500
CRATONVM_GC_STATS=1 tools/rss-by-mapping.sh -l cvm-8g    -- cratonvm -Xmx8g -cp $P Zr7RsBoot hello 1500        # expect = R1
CRATONVM_GC_STATS=1 tools/rss-by-mapping.sh -l cvm-g1    -- cratonvm -XX:+UseG1GC -Xmx256m -cp $P Zr7RsBoot hello 1500  # expect ≈ R1
CRATONVM_GC_STATS=1 tools/rss-by-mapping.sh -l cvm-gc    -- cratonvm -Xmx256m -cp $P Zr7RsBoot gc 1500         # + first collection
# R3 the allocator's own figures (mimalloc prints its stats at exit)
MIMALLOC_SHOW_STATS=1 cratonvm -Xmx256m -cp $P Zr7RsBoot hello 0 2>&1 | grep -iE 'reserved|committed|peak|rss'
MIMALLOC_PURGE_DELAY=0 tools/rss-by-mapping.sh -l cvm-purge0 -- cratonvm -Xmx256m -cp $P Zr7RsBoot hello 1500
# R4 THP (only meaningful when R0 printed [always])
CRATONVM_GC_STATS=1 CRATONVM_ZGC_ARENA_NO_THP=1 tools/rss-by-mapping.sh -l cvm-nothp -- cratonvm -Xmx256m -cp $P Zr7RsBoot hello 1500
# R5 per-class cost (in-process VmRSS delta; n=0 is the noise control)
cratonvm --nojit -Xmx256m -cp $P Zr7RsBoot classes 0    $NAMES
cratonvm --nojit -Xmx256m -cp $P Zr7RsBoot classes 3000 $NAMES
java -Xint -Xshare:off -XX:+UseZGC -Xmx256m -cp $P Zr7RsBoot classes 0    $NAMES
java -Xint -Xshare:off -XX:+UseZGC -Xmx256m -cp $P Zr7RsBoot classes 3000 $NAMES
# R6 positive control for the file classes (archives read instead of mapped)
CRATONVM_DISABLE_JAR_MMAP=1 tools/rss-by-mapping.sh -l cvm-nommap -- cratonvm -Xmx256m -cp $P Zr7RsBoot hello 1500
# R7 (optional, a separate build) allocation sites of the anon:other share
cargo build --release -p cratonvm-cli --no-default-features --features zgc   # system malloc
heaptrack ./target/release/cratonvm -Xmx256m -cp $P Zr7RsBoot hello 0       # if heaptrack exists
# R8 the jrt mount (maxrss; expect CVM ≈ +150 MB over its R1, HotSpot ≈ +20 MB over its R1)
/usr/bin/time -f '%M KB %U %S' cratonvm -Xmx512m -cp $P Zr7RsBoot list > /dev/null
/usr/bin/time -f '%M KB %U %S' java -XX:+UseZGC -Xmx512m -cp $P Zr7RsBoot list > /dev/null
```

How to read the rows:

- **R1:**
  - The script splits the anonymous total into `gc_heap_kb`, `gc_bitmaps_kb` (counted from
    pagemap over the ranges in the spans line), `stacks_kb`, `anon_exec_kb` and `anon_rest_kb`
    (the allocator).
  - Its `rss-class` lines give `exe:*`, `jdk:jimage`, `files:other` (the `.jmod` files) and
    `libs:*`.
  - **Positive control:** `rss-gc span=arena resident_kb` must be > 0 and ≤ `fp_arena_written`
    on the exit summary.
  - **Control that must stay zero:** the HotSpot rows have no `rss-gc` lines.
- **R2:** `cvm-8g` equal to R1 (within 2 MB) confirms "flat in -Xmx" on Linux, and `cvm-g1` about
  equal confirms "not the collector".
- **R4:**
  - It decides `CRATONVM_ZGC_ARENA_NO_THP`. Flip it to default-ON if, on an `always` host,
    `gc:java-heap anon_huge_kb > 0` on R1 and R4 shows a lower `gc_heap_kb`.
  - **Positive control:** the exit spans line says `arena_no_thp=1`.
  - **Kill-switch control:** R1 itself, which has `arena_no_thp=0`.
  - On a `madvise` / `never` host the row is vacuous by construction. Expect `anon_huge_kb=0` in
    both arms.
- **R5:** `per_class_b` against HotSpot's is the metadata multiplier. `classes 0` must give
  `rss_delta_kb` ≈ 0.

## Follow-up rows (lazy arena commit; z7 follow-up commit)

Host, `P` = compiled `tools/probes`, `NAMES` as in R0. Interleave the on and off arms, 5 pairs
each, and read CPU (user+sys) and maxrss from `/usr/bin/time -f '%M KB %U %S'`.

```
# F1 footprint: default (switch ON) vs kill vs env arms
CRATONVM_GC_STATS=1 tools/rss-by-mapping.sh -l cvm-lazy -- cratonvm -Xmx256m -cp $P Zr7RsBoot hello 1500
CRATONVM_GC_STATS=1 CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT=0 tools/rss-by-mapping.sh -l cvm-eager -- cratonvm -Xmx256m -cp $P Zr7RsBoot hello 1500
CRATONVM_GC_STATS=1 MIMALLOC_ARENA_EAGER_COMMIT=2 tools/rss-by-mapping.sh -l cvm-envwins -- cratonvm -Xmx256m -cp $P Zr7RsBoot hello 1500
CRATONVM_GC_STATS=1 MIMALLOC_ARENA_EAGER_COMMIT=0 CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT=0 tools/rss-by-mapping.sh -l cvm-envlazy -- cratonvm -Xmx256m -cp $P Zr7RsBoot hello 1500
# F2 throughput, interleaved on/off (on = unset, off = CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT=0)
/usr/bin/time -f '%M KB %U %S' cratonvm -cp $P Zr3JaRefArrayAlloc 1 10 64 ref 400
/usr/bin/time -f '%M KB %U %S' cratonvm -Xmx256m -cp $P MtChurnProbe 8 100 96
/usr/bin/time -f '%M KB %U %S' cratonvm -Xmx256m -cp $P Zr7RsBoot classes 3000 $NAMES     # class loading heavy
/usr/bin/time -f '%M KB %U %S' cratonvm -Xmx256m -cp $P Zr7RsBoot hello 0                 # boot CPU
```

How to read them:

- **F1 pass:**
  - `cvm-lazy` shows the summary line `[cratonvm] mimalloc: lazy_arena_commit=default
    arena_eager_commit=0`, total ≈ 150-160 MB, and `anon_huge_kb` ≈ 0 on the `[anon:mimalloc]`
    rows (check the `rss-top` lines).
  - `cvm-lazy` equals `cvm-envlazy`, the env-from-process-start arm, within noise. **That is the
    proof that the hook ran before the first arena.** If the first arena had been reserved
    before the hook, `cvm-lazy` would keep that 1 GiB huge-page arena and sit near `cvm-eager`.
- **F1 positive control:**
  - `cvm-eager` (kill) prints `lazy_arena_commit=killed arena_eager_commit=2` and is back at
    ~209 MB with ~141 MB huge.
  - `cvm-envwins` prints `lazy_arena_commit=env arena_eager_commit=2` and looks like `cvm-eager`.
- **F2:**
  - The switch stays default ON if CPU on every row is within the host's noise floor (8-40 %
    between runs; settle it with interleaved pairs) of the kill arm.
  - Lazy commit costs one `mprotect` per newly committed arena slice, and loses THP's TLB
    benefit on allocator memory. `MtChurnProbe` (allocator-heavy native side) and the `classes`
    row are where that would show.
  - **Refuted if** any row's median CPU is more than 5 % worse on the ON arm across 5
    interleaved pairs. In that case keep the switch, but consider `allow_thp=0` instead, or
    `arena_eager_commit=0` only for the first arena.

## What would close it

The page closes per owner. None of the owners is the GC.

- **classloading:** central-directory-only jmod indexing (expected: most of the `.jmod` file
  share), and per-class metadata near HotSpot's 4.7 KB.
- **allocator / VM:** name the ~40 MB remainder (R7), then fix its owners. mimalloc's retention
  (R3) is a configuration question.
- **NIO:** the `jrt:/` image buffer (R8).
- **JIT:** pack compiled bodies into shared code pages instead of one `mmap` each.
