# Proposal: key the unregistered-frame memo and the JIT scan cache by generation instead of clearing them before every scan

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 42
> of 54).** Not built. **Gate:** `CRATONVM_DBG_A5_CENSUS=1` on Tomcat start-up
> or `GenR4W4JitWarmDivertProbe`: worth it only if `A5_PROBE_FULL_RESCAN` is a
> visible share of native-call time. **Size:** S-M.

*Filed 2026-09-26 by gen round 5, wave 2, lane `roots6`. The design answer
to P1 of `../../internal/gc/gengc-r4-mark-conservative-stack-scan-gaps-RETIRED-20260928.md`.*

## Today

`invalidate_scan_cache_for_gc()` resets `UNREG_JIT_MEMO` to
`UnregMemo::new()` and empties `JIT_SCAN_CACHE`, and it is called immediately
before every production `scan_active_jit_frames` (`roots.rs` step 14, every
`update_root_snapshot` -- i.e. every object-returning native call and every
safepoint snapshot -- and `vm_exec.rs`). Both structures therefore never hit:
the unregistered-frame probe re-walks the whole band on every native call,
and the scan cache pays its fill (a `collection_count()` that snapshots the
heap's stats) for nothing.

The resets are correctness-motivated: a cached answer computed before a
collection, or before the chain changed, describes addresses that may no
longer hold what it says.

## Proposal

Replace "clear before use" with "valid only for the key it was computed
under", the key being `(heap collection count, chain generation)`:

- **Chain generation:** a per-thread counter bumped by every push/pop of
  `JIT_ENTRY_CHAIN` and every `flush_top_rbp_cache_to_chain` that changes an
  entry. A memo computed under generation `g` covers exactly the band the chain
  had at `g`.
- **Collection count:** the heap's `collection_count()` read ONCE per scan
  (the value `update_root_snapshot` already has), not per cache probe.
- A probe whose key matches reuses the verified band (`UnregScan::Detect`'s
  hi-water fast path, as documented); any mismatch falls back to the full
  rescan, exactly today's behaviour. The explicit `invalidate_scan_cache_for_gc`
  calls become generation bumps.

Correctness argument: every event that made the reset necessary (a moving
collection, a chain change, a stack band change) changes the key, so no stale
answer is ever served; what is kept is only an answer for an unchanged key.

## First step (measurement, before any code)

```bash
CRATONVM_DBG_A5_CENSUS=1 cratonvm ... <Tomcat start-up or GenR4W4JitWarmDivertProbe>
```

Read `A5_PROBE_FULL_RESCAN` against `A5_PROBE_MEMO_CLEAN` /
`A5_PROBE_MEMO_BANDED` (`conservative_roots.rs`). Today the memo counters are
~0 by construction; the proposal is worth doing if `A5_PROBE_FULL_RESCAN` is a
visible share of native-call time (`perf` on the census run).

## How to verify

Unit tests: a memo computed under key `k` is reused under `k` and discarded
under any other key; a chain push/pop bumps the generation. Runtime: the
census above shows `A5_PROBE_MEMO_*` > 0 with byte-identical program output
and an unchanged `[GC] decision histogram:` on
`GenR4W4JitWarmDivertProbe` (`PASS jitwarm threads=4 calls=1500000
checksum=-4668312146048759300`).
