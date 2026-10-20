# ZGC: collection time per allocated byte is ~80x HotSpot's on a lean array allocation loop

Status: OPEN (filed 2026-10-19 by the JIT round 21 orchestrator; GC-side, out of the JIT round's scope)
Area: GC (ZGC default collector), Windows x86-64
Found by: JIT round 21 wave 2, lane alloc's census on `cvm-r21w2.exe` (release, fat LTO, `claude/jit-compiler-improvements-181840`)

## Measurement

`tools/probes/jit/r21/R21AllocLean.java`, single row (`R21AllocLean lean-static-long16`: `new long[16]` stored to a
static, 18M allocations), `-Xmx512m`, ns per allocation:

| | total | cpu | gcms (collection ns / allocation) | gcs (collections / 1M allocations) |
|---|---|---|---|---|
| CratonVM ZGC | 28.3 | 24.3 | 26.0 | 0.67 |
| HotSpot 25 `-XX:+UseZGC` | 12.2 | 13.9 | 0.3 | 1.56 |

CratonVM runs fewer collections (0.67 vs 1.56 per million allocations) but each costs ~200x more, so collection
time is ~26 of the 28 ns. `lean-static-byte16` (32-byte objects): gcms 4.3 vs total 6.2.

The JIT side is not it: the census (`CRATONVM_DBG_JIT_METHOD_STATS=1`) shows the compiled bump engaged
(`jit_newarray_calls=135235` of 18M, `tlab_refills=5609`), and the compiled sequence for `long[16]` differs from
`byte[16]`'s by one `SHL` (lane alloc, `r21-lean-allocation-rows-cost-is-outside-the-inline-sequence-20261018.md`).
Generational shows the same row at 36.6 ns.

## Next step (GC round)

Run the row with `CRATONVM_GC_STATS=1 --verbose:gc` and split one cycle's time (mark / relocate / page
reclaim / per-cycle O(heap-capacity) passes -- see the memory note on O(capacity) passes at the default 16 GiB
`-Xmx`, here 512m). An all-garbage young cycle should cost close to nothing.
