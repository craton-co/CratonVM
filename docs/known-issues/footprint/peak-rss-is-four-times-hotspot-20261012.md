> **MOVED (20261019, zt round orchestrator):** out of the ZGC set: its open part belongs to another owner (zt t5/pg3 audit, `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md`). Former name `docs/known-issues/footprint/peak-rss-is-four-times-hotspot-20261012.md`.

# ZGC: peak RSS is ~4x HotSpot ZGC's at the same -Xmx (three separable parts)

> **STATUS (2026-10-19, zt t5/pg3): OTHER-OWNER (VM footprint).** Parts 2 and 3 (the GC's) are closed on the host (zr z3b, zs s1a R3). Part 1, the non-GC boot baseline (~210 MB vs ~44 MB), belongs to the boot-image / metadata / allocator owners. Move to `docs/known-issues/footprint/peak-rss-is-four-times-hotspot-20261012.md`; its Linux maxrss rows and the Windows PeakWorkingSet form travel with it.

> **STATUS (2026-10-19, zt t3/fp): part 1 OPEN.** Two small default-ON cuts (jimage directory records, padded-code memo eviction) and the t2 opt-in levers; every row now has a Windows form: PeakWorkingSetSize of the process (PowerShell `Start-Process -PassThru` then `.PeakWorkingSet64` after `WaitForExit`), which stands in for Linux `/usr/bin/time -f %M` (both are the peak resident set; Windows counts the working set, so compare arms on one OS only, never across). Rows: `docs/internal/zgc-round-zt-20261018/t3-fp-report.md`.

> **STATUS (2026-10-19, zt t2/fp): part 1 OPEN; its unmeasured levers are batched.** Part 1 (the non-GC boot baseline, hello ~210 MB vs HotSpot ~44 MB) has five opt-in levers that no host row has measured, now all arms of one interleaved spec (`t2fpab.spec`: ST / STS, SB, LD, FH, BM, and ALL = SB + ST + LD); the new one is `CRATONVM_VTABLE_LAZY_DISPATCH` (dispatch snapshots on first use, ~1.9 KB a linked class by the local census). Rows: `docs/internal/zgc-round-zt-20261018/t2-fp-report.md` and `t2fpab.spec`.

> **STATUS (2026-10-16, zs s10/pg2): OPEN -- part 1 (the non-GC boot baseline) stays open; fp3 is now host-measured: ab-s3a cls RSS D 245 MB vs s2b 257 MB (-12 MB, CPU equal). s6a measured two opt-in boot levers at flat CPU: `CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP=1` -13 to -15 MB and `CRATONVM_TIER_C2_THREADS=1` -7 to -13 MB. Lane rs2 (s9) builds on them, but its build s8a is queued and not run. Parts 2-3 still rest on zr z3b and s1a tp R3, not on this page's own rows.**

> **STATUS 2026-10-15 (zs s5/pg): PARTLY -- Parts 2-3 are largely closed on the host: zr z3b "RSS growth over a burst now close to HotSpot (1g +892 vs +830 MB)", and the warm-up rule fires at 10/20/30 % (zs s1a tp R3). Part 1 remains, the non-GC boot baseline: zs ab-s1bf hello RSS is 218848 KB vs HotSpot ~44 MB. `CRATONVM_MIMALLOC_FIRST_TOUCH=lazy` saves 71-88 MB but failed the CPU bar (hello +23 %). fp2 saved -11 MB on the host; fp3 is not host-measured.**

> **STATUS (2026-10-14, zs s1/fp): part 1 (non-GC owners) has two new levers, PENDING HOST PROBE.** The per-class metadata is -1.0 KB a class in code (packed verifier type maps). The mimalloc arena's first touch has an opt-in policy, `CRATONVM_MIMALLOC_FIRST_TOUCH`. Rows: the s1-fp report.

> **STATUS (2026-10-13, zr z11/md): part 1's per-class share reduced in code, PENDING HOST
> PROBE.** The per-class class-metadata cost (z8/fm's ~25 KB of allocator a class) is 16.4 KB
> in the deep census after z11/md (-31%): 32-byte attribute slots, a compact per-class code
> buffer (`CRATONVM_CLASS_CODE_COMPACT`) and no cached class file for boot class path classes
> (`CRATONVM_BOOT_CLASS_BYTES_REFETCH`). See
> [`zgc-zr8fm-class-metadata-per-class-cost-20261013.md`](../../internal/gc/zgc-zr8fm-class-metadata-per-class-cost-20261013.md);
> host rows in `docs/internal/zgc-round-zr-20261012/z11-md-report.md`. A Hello world loads
> ~940 classes, so the expected boot saving is ~7 MB of anonymous memory.

> **STATUS (2026-10-13, zr z7/rs): part 1 decomposed, still OPEN.**
> [`boot-rss-by-owner-20261013.md`](boot-rss-by-owner-20261013.md) splits the
> boot baseline by owner. Locally (Windows working set, Hello world, 151 MB) it is:
> - the process allocator: ~81 MB;
> - the 70 mapped `jmods/` files: ~47 MB, because `load_jmod` reads every entry's local header;
> - the binary: ~17 MB;
> - the GC: ~2.5 MB (arena plus bitmaps).
>
> Host rows R0-R8 are on that page, with the new `tools/rss-by-mapping.sh` and
> `Zr7RsBoot`. Parts 2 and 3 are unchanged by z7.

> **STATUS (2026-10-12, zr z3/fp): PARTLY FIXED IN CODE, PENDING HOST PROBE; stays OPEN for parts 1 and 3.** GC-side pieces fixed: the uncommit floor was the startup commit, which the ergonomic initial heap made `-Xmx` on large hosts, so nothing was ever uncommitted (`../../internal/gc/zgc-zr3fp-uncommit-floor-was-the-initial-heap-FIXED-20261012.md`, `CRATONVM_ZGC_UNCOMMIT_TO_MIN_HEAP`); the forwarding table (up to ~200 MB of buckets), the dead scratch (up to ~100 MB) and the mark bitmap's pages kept their peak (`zgc-zr3fp-side-structures-kept-their-peak-20261012.md`, `CRATONVM_ZGC_FOOTPRINT_TRIM`, `CRATONVM_ZGC_SPARSE_BITMAP_CLEAR`); the uncommit clock started a collection late (`CRATONVM_ZGC_UNCOMMIT_STAMP_AT_SWEEP`). Filed: `zgc-zr3fp-slide-transients-scale-with-objects-moved-SUPERSEDED-20261019.md` (a slide holds ~3 copies of the moved set: the likeliest GC-side share of the small rows' excess), `../../internal/gc/zgc-zr3fp-no-uncommitter-an-idle-heap-never-gives-back-FIXED-20261012.md`. New instrument: `[GC] zgc-footprint:` (`CRATONVM_DBG=gc-stats`) prints committed/peak/ever-written arena bytes and every side structure's bytes; new probe `Zr3FpRss` (`idle | burst | steady`, VmRSS/VmHWM from `/proc/self/status`, HotSpot twin is the same source). Part 2 below is likely already gone on today's tree (bounded snapshot); the host's `fp_snapshot_peak` decides. Rows: `docs/internal/zgc-round-zr-20261012/z3-fp-report.md`.
>
> **STATUS (2026-10-12, zr z2/cp): OPEN, decomposed, not fixed.** Measured locally on the 2026-10-03 release binary (Windows peak working set), so the absolute figures differ from the host's Linux maxrss (~500 MB for `MtChurnProbe 4 60 48` at `-Xmx256m` on base0). The decisive host rows are below; part 2 may already be smaller on today's tree (the 2026-10-07 run-based registry snapshot).

Area: footprint. Not a correctness defect, but `-Xmx256m` is not a 256 MB
process on either collector here, and the ZGC arm is the worst of the three.

## Measurements (local, peak working set)

| run | CratonVM ZGC | HotSpot 25 ZGC | CratonVM G1 |
|---|---|---|---|
| `BootIdle` (prints `maxMemory`), `-Xmx256m` | 153 MB | 39 MB | |
| `BootIdle`, `-Xmx8g` | 150 MB | | |
| `BootUsed` (one `System.gc()`), `-Xmx32m` | 157 MB | | |
| `BootUsed`, `-Xmx256m` | 174 MB | 39 MB | 162 MB |
| `BootUsed`, `-Xmx2g` | 235 MB | | |
| `BootUsed`, `-Xmx8g` | 602-650 MB | | 192 MB |
| `BootUsed`, `-Xmx8g`, `CRATONVM_ZGC_RELOCATE=0` | 537 MB | | |
| `MtChurnProbe 4 60 48`, `-Xmx256m` | 410-415 MB | 103 MB | |

(`BootIdle` / `BootUsed` were scratch classes; they are checked in as
`tools/probes/Zr2CpBootFootprint.java` `idle` / `gc`.)

## The three parts

1. **Non-heap boot baseline: ~150 MB vs 39 MB**, flat in `-Xmx` (`BootIdle`
   at 256m and 8g are equal), and also ~125 MB under `--compatible`. Not the
   collector: class metadata, the JIT, the interned boot image. Owner outside
   this round.
2. **A capacity-proportional cost of the FIRST collection, ZGC only: ~55 MB
   per GB of `-Xmx`** (`BootUsed` 8g minus `BootIdle` 8g = ~450 MB; G1 at 8g
   pays ~40 MB). Present with relocation off, so it is the mark / sweep /
   snapshot path, not the slide: something writes (or allocates and touches)
   ~1/18 of capacity once per cycle. 1/64-of-capacity tables (the
   object-start bitmap, the mark bitmap, a full-length snapshot `Vec`) at
   three to four copies would explain it. The 2026-10-07 change in
   `gc/src/zgc/starts.rs` (`ZSnapWords::from_runs`: "the bounded snapshot ...
   still returned a full-length zeroed `Vec`") postdates the measured binary
   and may be most of it; the host row below decides. At `-Xmx256m` this part
   is ~15 MB.
3. **The whole arena is touched before the first cycle.** `MtChurnProbe` at
   `-Xmx256m` runs ONE collection, `GC(0) Garbage Collection (Allocation
   Failure) 191M->32M(196M)`: no Warmup cycle at 10/20/30 % of capacity as
   HotSpot runs, so the bump pointer walks ~200 MB of fresh pages, and the
   uncommit delay (300 s) keeps them. HotSpot's peak for the same run is
   103 MB total. This is the same trigger finding lane tr recorded in z1
   (our causes are `Allocation Stall`, HotSpot's are `Warmup` then
   `Allocation Rate`).

So at `-Xmx256m`: ~150 (1) + ~15 (2) + ~250 (3) = the ~415 MB measured.

## What would close it

- Part 3 is a trigger change (lane tr's area): HotSpot's warmup rule
  (collect at 10 %, 20 %, 30 % of `-Xmx` during the first cycles) would cap
  the touched arena at the live set plus one cycle's allocation. The A/B is
  `MtChurnProbe 4 60 48` maxrss and CPU, default vs a warmup arm.
- Part 2: name the table. `perf record -e page-faults` (or `minflt` from
  `/usr/bin/time`) on `BootUsed -Xmx8g` with and without the `System.gc()`,
  and the same with `CRATONVM_ZGC_BITMAP_BOUNDS=0`, separates the bounded
  tables from the unbounded one.
- Part 1 belongs to the VM's boot-image / metadata owners.

## Rows for the host (today's binary)

```
/usr/bin/time -f '%M KB %U %S' cratonvm -Xmx256m Zr2CpBootFootprint idle
/usr/bin/time -f '%M KB %U %S' cratonvm -Xmx8g   Zr2CpBootFootprint idle
/usr/bin/time -f '%M KB %U %S' cratonvm -Xmx8g   Zr2CpBootFootprint gc   # one System.gc()
/usr/bin/time -f '%M KB %U %S' cratonvm -XX:+UseG1GC -Xmx8g Zr2CpBootFootprint gc
/usr/bin/time -f '%M KB %U %S' cratonvm -Xmx256m -Xlog:gc MtChurnProbe 4 60 48
/usr/bin/time -f '%M KB %U %S' java -XX:+UseZGC -Xmx256m -Xlog:gc MtChurnProbe 4 60 48
```

Read: `gc 8g - idle 8g` is part 2 (positive control: it must exceed
G1's difference; if it does not, part 2 is already fixed on today's tree and
this page shrinks to parts 1 and 3). The `-Xlog:gc` cycle count of the churn
row against HotSpot's is part 3.
