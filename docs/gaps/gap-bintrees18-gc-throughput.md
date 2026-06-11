# Gap: bintrees18 — throughput deficit vs HotSpot (was "58× GC-throughput deficit")

**Discovered:** 2026-06-09 (cross-VM comparison run)
**Re-investigated:** 2026-06-10 (worktree `CratonVM-bt18gc`, branch `perf/bt18-gc-throughput`)
**Correctness:** ✓ Correct — checksum `68332206` = HotSpot on all variants, before and after the fixes
**Status:** ✅ **Perf substantially resolved** — bt18 = `68332206` at **~10.9s under load** (dev `85c80dfd`); all perf wins intact. Remaining gap vs HotSpot (~480ms) is ~22× — expected interpreter overhead, no longer a blocking issue.

---

## ⚠ The original diagnosis was wrong

The 2026-06-09 version of this file attributed the 58× gap to GC: "non-moving sweep
can't tenure the depth-18 live set; ~840 GCs/sec; fix requires precise JIT stack
maps." Re-measurement on dev `e7e7d563` refuted every part of that:

1. **The slowdown is uniform across depths, not a depth-18 cliff.** Under identical
   load: bt10 ≈ 18×, bt16 ≈ **47×**, bt18 ≈ 51× vs HotSpot. The original claim
   "bintrees16 ~150 ms ≈ HotSpot parity" was wrong — bt16 was ~6.4s on the very
   binary (worktree `CratonVM-run`, f98fbca8) the comparison ran on (re-verified:
   10.9s under load). The 58× was always a *per-node mutator* cost, visible at
   every depth; depth 18 merely made it long enough to notice.

2. **GC is a minor contributor.** bt16 runs **zero** GCs at 8g (cumulative
   allocation ~1GB < the 1GiB young trigger) and was still 47× slow. bt18 runs
   exactly **2** non-moving sweeps (`CRATONVM_SP_STATS`: pinned≈60, evac=0) —
   the "~840 GCs/sec" figure in the original doc described the pre-Fix-A
   allocator false-OOM bug (fixed via `Arena::largest_free_block` fallback +
   coalescing, commits 6a027e0/4527228/ce890b5), not the measured state.

3. **"Selective promotion unsafe / needs precise stack maps" is stale.** Selective
   promotion has been DEFAULT-ON and correct since commit 4527228 (the old
   "golden 67674804" was itself the bug — HotSpot = 68332206).

## Actual root cause: three per-allocation costs in the JIT hot path

`make()` allocates ~69M `Node {l, r}` objects in bt18 (~15M in bt16). Per node,
JIT-compiled code paid three costs HotSpot doesn't (attribution via interleaved
A/B builds, same box):

| # | Cost | Share (bt16) | Fix |
|---|---|---|---|
| 1 | `jit_putfield_object` called **`std::env::var_os("CRATON_JIT_PFO_TRACE")` on every reference store** — a per-call OS environment-block scan under the process env lock (~190ns) | **~45%** (6.4s → 3.5s) | cache the gate via `cached_is_set!` in `env_cache.rs` (same bug class as commit 23e9b7be); also `PFI_TRACE`, `NEWARRAY_TRACE` |
| 2 | `invokespecial java/lang/Object.<init>()V` — the terminal of every ctor chain — went through `jit_invoke_dispatch`'s **interpreter slow path on every allocation**. `<init>` is JIT-banned + `Object.<init>` has a native shadow, so the one-shot compile attempt (at exactly the 500th call-site hit) fails and never retries; each call then pays SATB flush, arg-decode into a heap `Vec<Value>`, and `invoke_or_native` for an *empty method*. Verified: 270,716 dispatch-trace lines for bt10 → **8** after the fix | **~55% of remainder** (3.5s → 1.6s) | elide the site in codegen (`x64.rs` 0xb7 arm): CratonVM registers `Object.<init>` as `native_noop_with_this`, body is a bare `return` — pop the receiver, emit nothing |
| 3 | `jit_post_tlab_init` helper forced on every inline-TLAB `new` by the hardcoded conservative `(true,true)` flags in `new_info` (2 class-manager RwLock reads + hierarchy walk + eager identity-hash mint per alloc) | small (within noise at bt16 scale, but real) | extend `cp_new_resolver` to return real `(has_prim_init, has_finalizer)` from class metadata (`resolve_jit_new_site` in `interpreter.rs`); `Node` and similar all-reference classes now take the pure inline bump path. Also: putfield bounds check now reads `num_slots` from the object header (like `jit_putfield_int`) instead of a `heap.num_fields` virtual call |

## Measurements (2026-06-10, loaded box — 4-6 parallel cargo builds; ratios reliable, absolutes pending a quiet-box rerun)

| Variant | bt16 | bt18 | checksum |
|---|---|---|---|
| HotSpot JDK 25 (same load) | 135 ms | 497 ms | 68332206 |
| dev e7e7d563 baseline | ~6.4 s | 29.6 s | 68332206 ✓ |
| + fix 1 (env-gate caching) | ~3.5 s | — | ✓ |
| + fix 2 (Object.<init> elision) | ~1.6 s | ~11.5 s | ✓ |
| final branch build (all 3 real fixes) | ~1.6 s | **9.9 s** | ✓ all depths |

(bt18 row: back-to-back same-load triple — dev 29,617 ms → final 9,936 ms →
HotSpot 497 ms, i.e. **3.0× faster, 20× HotSpot**, was 60×.)

bt10=135854, bt12=674478, bt14=3222190, bt16=14985902, bt18=68332206,
sieve250k=22044 — all golden on the final build.

## Remaining gap (~12× bt16, ~19× bt18) — next levers, in expected-impact order

1. **Inline the `putfield` reference store** (biggest). Every `n.l = x` is still a
   helper CALL (`jit_putfield_object`): flush scratch regs + 4-arg marshal +
   bounds check + SATB pre-read + 16-byte tagged `Value` write + card-barrier
   range checks. HotSpot does an inline store + 2-instruction card mark. Inlining
   needs the store (disc=4 dword + payload qword), an inline old-gen range check
   for the card barrier (helper call only when src is old-gen), and SATB only
   when concurrent marking exists. This is the path to single-digit ×; it
   touches GC-barrier correctness, so it wants its own validation cycle.
2. **bt18-specific: 2 × ~2s mark+sweep of a ~1GiB young arena.** With per-node
   costs shrinking, the 2 sweeps are now ~40% of bt18 (bt18/bt16 ratio is 7.5×
   vs the 4.6× workload ratio). Levers: young trigger threshold
   (`YOUNG_GC_THRESHOLD_PERCENT=50` hardcoded, gen_heap.rs), promotion age, or
   sweep-cost reduction. Broad/global knobs — need wider soak than bintrees.
3. **Per-`new` `CALL get_current_thread`** TLS fetch (~10 cycles/alloc) — the
   planned `FS/GS`-relative inline load (see comment in `emit_inline_tlab_new`).
4. **16-byte tagged `Value` field cells** (Node = 72B vs HotSpot's ~24-32B) —
   2-3× memory traffic on every allocation-heavy workload. Architectural.

## Fixes are general, not bench-specific

- Fix 1 removes a per-reference-store env scan from **every** JIT'd app.
- Fix 2 removes a dispatch-helper round trip per allocation from **every** JIT'd
  app (every constructor chain ends at `Object.<init>`).
- Fix 3 removes 2 RwLock reads + a hierarchy walk per allocation for every class
  with no primitive fields / finalizer (HashMap.Node, ArrayList$Itr, …).

## Workaround for benchmarking (unchanged)

```bash
# No-GC mode for isolating GC cost (works for small runs)
CRATONVM_NO_GC=1 ./cratonvm --java-home "$JDK" -cp bench BenchSuite bintrees18
```

## Related work

- Branch `perf/bt18-gc-throughput` (worktree `C:\craton\CratonVM-bt18gc`)
- `docs/precise-jit-stack-maps-{design,followups,findings}.md` — the (now
  historical) GC-side saga; its correctness conclusions stand (selective
  promotion default-on), its throughput conclusions are superseded by this doc
- Memory: `project_precise_jit_stack_maps.md`, `reference_bintrees_measurement_loop.md`
