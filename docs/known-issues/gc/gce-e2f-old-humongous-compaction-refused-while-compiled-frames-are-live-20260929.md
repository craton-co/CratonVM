# Generational: the humongous compaction an allocation needs is refused whenever compiled frames are live

> **STATUS (2026-09-29, gce e2/f): OPEN -- named and measured on the base; no
> code change.** Takes over the Generational residual of
> `gcd-d10f-native-growth-reclaim-osr-main-holder-unnamed-20260928.md`,
> item 3 of `common-w34-...` and all of
> `gce-e1f-windows-native-growth-alAdd-fails-with-the-jit-SUPERSEDED-20260929.md`.
> Owners: the Generational old-gen code (`gc/src/gen_heap.rs`,
> `OldPinnedCompact`) and the root doors that publish `OldPinRootLayout`;
> the frames lane for the rewrite half of the proposal below.

*Filed 2026-09-29 by gce wave e2, lane f.*

## Evidence

Base `adb9178bc`, Windows, `-XX:+UseGenerationalGC -Xmx128m -cp
tools/probes NativeGrowthReclaimProbe alAdd` (the row that also fails on
Linux, `gen_ngr_census` `alAdd=1/4`):

- A failing run with `CRATONVM_DBG=oldmark-root-census,root-source
  CRATONVM_GC_STATS=1 RUST_LOG=cratonvm::gc=info`: at each missed round the
  one large live old object is the round's own `ArrayList` array (26 MiB),
  reached from the running IR `ArrayList.add` frame (`this`, `in_map=true`)
  and the grow native's `native-pin`: live, not retained. The grow's 39 MiB
  request fails with `old_free_bytes=40322688 old_largest_free_block=27709728`.
- Each failing request is followed by
  `old-gen humongous compaction not taken by default; reclaiming in place
  why="conservative roots are live (CRATONVM_GC_OLD_PINNED_COMPACT is off)"
  published_words=4..6 pinned_root_range=3890..4006`.
- `--nojit`: `alAdd=4/4` 3/3 (the default humongous compaction runs:
  nothing conservative is live).

| Arm (`alAdd`, 4 rounds per run) | Result |
|---|---|
| default | 1/4, 1/4 (and 4/4 in other runs) |
| `CRATONVM_GC_OLD_PINNED_COMPACT=1` | 3/4, 3/4, 2/4, 3/4, 2/4, 3/4 |
| `-XX:NewRatio=2` | 0/4, 1/4 |
| `-XX:NewRatio=2 CRATONVM_GC_OLD_PINNED_COMPACT=1` | 4/4, 4/4, 4/4 |
| `--nojit` | 4/4, 4/4, 4/4 |

## Why

1. `gen_heap.rs`, the non-moving sweep's `humongous_refusal` arm: the
   default compaction for a refused humongous allocation is taken only when
   `OldPinnedCompact::for_this_pause(..).pins_nothing_conservative()`. With a
   compiled frame on any stack the band words are published, so it never is;
   the opt-in pinned compaction is the only other route.
2. `OldPinnedCompact::plan_from_sources`: with the interpreter probe live
   (`gc_quiescence::compiled_frames_live()`) and no `OldPinRootLayout` from
   the door, the WHOLE root slice is pinned (`pinned_root_range` ~ the root
   count above). So even the opt-in pins the round's list array (a root in
   that slice names it) and cannot open a 39 MiB hole beside it.
3. Geometry: the default Generational old gen is 64 MiB of `-Xmx128m` (young
   is a 32 MiB semi-space pair). 26 MiB live + 39 MiB = 65 MiB, so in that
   geometry the row passes only while the list array is still young. HotSpot
   Serial's old gen is 85 MiB there.

## What would fix it

- `gengc-r5w6-old10-proposal-pinned-compaction-pins-only-unrewritable-words-20260927.md`:
  pin only the objects UNREWRITABLE words name, and rewrite map-named
  compiled-frame words (and register images) after an old-gen move, as the
  moving young cycle does.
- The allocation-ladder door publishes an `OldPinRootLayout` for its root
  slice, so only the peers' part pins wholesale (`OLD_PIN_LAYOUT_FALLBACKS`
  counts the misses).
- Then the default humongous compaction can be allowed with published
  words present, since they no longer pin movable objects.

## How to verify

`NativeGrowthReclaimProbe alAdd` on Generational, `-Xmx128m`, default
flags: 4/4 in 6/6 runs on Linux and Windows, with no `humongous compaction
not taken` line; `GenR4W5OldPinnedCompactProbe` and the JIT OOME battery
unchanged. Until then, the frames question of the NGR page is judged in
HotSpot's geometry with the opt-in:
`-XX:NewRatio=2 CRATONVM_GC_OLD_PINNED_COMPACT=1`.
