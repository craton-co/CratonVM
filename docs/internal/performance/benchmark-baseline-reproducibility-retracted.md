# Baseline-reproducibility investigation (retracted)

Split out of `BENCHMARK.md`. The hashmap arm of this investigation does not
reproduce, and the reasoning that generalised from it to the other rows does
not stand. Kept unedited for provenance; do not build on any absolute number
in it without re-measuring.

---

> **⚠ THE 2026-07-25 MEASUREMENTS BELOW ARE UNRELIABLE — READ THIS FIRST
> (added 2026-07-30).** The hashmap arm of this investigation was re-run from
> scratch on 2026-07-30 and **nothing in it reproduces**. Where this block
> reports 22.3 s "under *every* methodology tried" and "nothing reproduces 4237
> ms", the same phase on `dev` @ `9ac1feffe` measures **4,065 ms** (median of 5,
> cpu 15, `-Xmx8g`, interleaved against HotSpot at 1,062 ms), and n=1M measures
> 427 ms against the 3,084 ms recorded here. The 4,300 ms baseline this block
> calls unreproducible is reproduced within 6%.
>
> Since the hashmap arm is wrong by ~5x, **the reasoning that generalised from
> it to the other three rows does not stand either.** Sieve, stringregex and
> bintrees were not re-measured as part of that work and are simply unknown;
> a fresh 5-rep interleaved run of all three on 2026-07-30 put bintrees at
> 1,635 ms (against the 2,023 ms "quiet median" below and its own 1,550 ms
> anchored baseline) and stringregex at 229 ms (against 457 ms below), which is
> consistent with the whole series being inflated rather than with four
> independent row-specific defects.
>
> The cause was NOT identified. The candidate explanation — that the gate's
> default cpu 13 pin collides with other sessions' benchmarks (see the
> methodology warning at the end of this block) — was tested directly on
> 2026-07-30 with the same binary alternating cpu 13 and cpu 15, and showed no
> difference (4131/4004 against 4063/4042). **Do not build on any absolute
> number in this block without re-measuring it.** See
> `hashmap-half-gap-20260730.md`.
>
> The original 2026-07-25 text is kept below, unedited, because it documents
> what was believed and how it was argued.
>
> ---
>
> **⚠ OPEN: 4 of the 7 baselines are not reproducible from the commit they were
> anchored at. It is NOT host load and NOT a regression.** Resolved 2026-07-25 on
> the Azure EPYC host; both earlier candidate explanations are refuted by
> measurement.
>
> **Not host load.** The gate was run at its documented defaults (cpu 13, `-Xmx8g`,
> median of 5) on a genuinely quiet host — 1-min load 1.10, every core idle in
> `mpstat`, zero competing benchmarks. The same 4 phases still fail, and the
> run-to-run spread collapses from the 33% seen under load to **under 1%**
> (hashmap: 22289/22312/22240/22317/22439 ms), which is itself the proof the host
> was quiet:
>
> | phase | quiet median | budget | verdict |
> |---|---|---|---|
> | arithmetic | 4067 ms | 4935 | PASS |
> | fib | 4442 ms | 4672 | PASS |
> | matrix | 2859 ms | 5985 | PASS |
> | sieve | 11655 ms | 6090 | **FAIL 1.9x** |
> | hashmap | 22312 ms | 4515 | **FAIL 4.9x** |
> | stringregex | 457 ms | 173 | **FAIL 2.6x** |
> | bintrees | 2023 ms | 1627 | **FAIL 1.24x** |
>
> That arithmetic/fib/matrix *pass* — matrix by 2x — on the very same runs proves
> the core is delivering full throughput. Uniform CPU starvation cannot produce a
> pass/fail split that is stable across load levels.
>
> **Not a regression.** `e57f0bc7d` (the commit the baselines were anchored at)
> was built with fat LTO and run interleaved against `58c9b643c` on an idle core.
> It fails the *same* 4 phases with statistically identical numbers — anchor
> hashmap 26350 vs dev 26075, anchor stringregex 463 vs dev 468. Where the two
> differ, **dev is faster**: bintrees 2101 vs 2895 (−27%), sieve 9029 vs 11597
> (−22%), i.e. the layout-registry/inline-TLAB work is measurably paying off.
> There is no regression to bisect.
>
> **Therefore the baselines themselves are wrong for these 4 rows.** They are
> marked `provisional` and were recorded as *"pair-1 best"* — a best-of, not a
> median — and evidently came from a different measurement series than the gate
> performs. hashmap in particular is ~22.3 s under *every* methodology tried
> (isolated cold, all-phase warm in-process at 23.5 s, `-Xmx8g` and `-Xmx2g`), at
> *both* commits. Nothing reproduces 4237 ms.
>
> **Do not re-anchor these baselines just to make the gate go green** — but note
> the reason has changed: the open question is no longer "is dev slow?" (it is
> not) but "where did 4237/5800/165/1550 come from, and on what?". Re-anchoring
> requires answering that first, under the README's evidence-doc policy. The
> `anchored` bintrees row is the one with a real evidence doc
> (`bt18-inline-tlab-regression-20260724.md`, 1527–1533 ms); at 2023 ms quiet it
> is 1.24x off its own doc and is the most tractable thread to pull.
>
> *(End of the 2026-07-25 text. The hashmap row was re-anchored to 1,800 ms on
> 2026-07-30 with the evidence doc the policy above asks for — see the banner at
> the top of this block for why the "nothing reproduces 4237 ms" premise no
> longer holds. The other three rows are untouched.)*
>
> **Where hashmap's 22 s actually goes** (perf, `-F 199`, quiet core): it is not
> the layout registry that `994a543bf` fixed for bintrees — that symbol does not
> appear. The profile is *entirely interpreter dispatch into the synthetic native
> collections*: `NativeMethodRegistry::find` 8.3%, the synthetic HashMap engine
> (`DenseIntEntries::note_fresh_insert` + `try_hm_int_fast_put`) 9.7%,
> `execute_invokestatic`/`execute_invoke_kind`/`resolve_method_metadata` ~11%, the
> per-call native-vs-bytecode policy checks
> (`synthetic_stub_should_yield_to_real_bytecode` +
> `should_force_registered_native_over_bytecode`) 5.2%, `OrderedPlRwLock::read`
> 2.6%. `hashMapPutGet` is entered *once* with two 10M-iteration loops, so
> invocation-count tier-up can never fire on it.
>
> **~11% of hashmap CPU was pure waste in `getenv` — now FIXED** (−13.1% on
> hashmap, −15.4% on stringregex; see the note below this one).
>
> **Methodology warning for whoever picks this up:** the gate's default pin is
> **cpu 13**, and concurrent sessions on this shared host run their own
> CratonBench pinned to the same cpu 13. Two benchmarks then timeshare one core
> while `mpstat` shows 14 other cores idle — contention far worse than the 1-min
> load average suggests, and `--max-load` does not catch it. Check
> `taskset -cp <pid>` on any competing `CratonBench` and use `--cpu N` on a
> verified-idle core, or wait for `pgrep -f CratonBench` to come back empty.

> **⚠ The gate CANNOT see the parallel young GC — it measures it at its worst
> configuration.** `regression-suite/perf/run-cratonbench-gate.sh` (line 89) pins with `taskset -c $CPU`, and
> the Rust `available_parallelism()` call honours the affinity mask, so
> `young_gc_threads` resolves to **1** on every gated run. Both the parallel
> mark drain and the parallel sweep are therefore disabled for the numbers the
> gate reports. A change that only helps multi-core GC will read as flat, or
> slightly negative from its added bookkeeping, on the gate — and a GC
> regression that only bites multi-core will not be caught at all.
>
> Before concluding a GC change did nothing, re-measure unpinned (or pinned to
> a CPU *set*, e.g. `taskset -c 8-15`) with `CRATONVM_DBG_GCPHASE=1` for the
> phase table. Worked example: the allocator-anchor change below measures
> −13.8% wall on `taskset -c 8-15`, and its headline 241 → 0 ms phase win is
> invisible to a single-CPU gate run.

> **✅ FIXED 2026-07-25: the young-GC mark-oracle walk (241 ms/collection) is
> gone**, replaced by allocator-recorded object-grid anchors (`gc/src/arena.rs`,
> `gc/src/gen_heap.rs`; merged `26e866b82`).
>
> The sweep parallel split points used to be a by-product of a full-arena
> exact-base walk. That walk chased 2 147 483 592 bytes of headers per
> collection. Anchors are now recorded by the allocator as objects are handed
> out — one verified start per 4 KiB bucket — so the grid is known without
> rediscovering it, and the remaining conservative-candidate oracle visits only
> intervals that contain a candidate: **4 718 536 bytes walked, a 455× drop**.
>
> Interleaved A/B, 12 pairs, `taskset -c 8-15`, quiet host: bintrees-18 median
> **1777 → 1531 ms (−13.8%)**, non-overlapping ranges, B won 12/12. Phase table
> (`CRATONVM_DBG_GCPHASE=1`): `mark-oracle-walk` **241 → 0 ms**, young GC total
> **419 → 170 ms**. Single-CPU (parallel sweep disabled): 2033 → 1800 ms wall,
> GC 916 → 393 ms.
>
> **The obvious alternative was measured, not assumed.** Dropping the
> `cand_idx` early-exit so the walk covers the whole arena and the sweep
> parallelises unconditionally is a **no-op on this workload**: the bintrees-18
> 126 candidates already spanned the full 2 GiB, the walk already reached
> `used`, and the sequential sweep tail was already 0 ms. It would have bought
> nothing while leaving the 241 ms in place. Allocator-sourced anchors make the
> grid independent of the candidate set entirely, which subsumes it.
>
> Correctness: 336 checksum-verified runs, 0 mismatches (7 multi-collection
> configs × 4 `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` values × 4 worker counts × 3
> reps, each compared against the baseline binary own answer), plus a new
> 8-thread mixed-size-class stress matching HotSpot 12/12, and all 7 CratonBench
> phase checksums exact.
>
> Known residual: with full-arena anchors, a single mid-arena grid anomaly now
> aborts the *whole* parallel sweep rather than truncating the prefix. Still
> correct (it falls back to the sequential walk) and never observed across 336+
> runs, but it is a sharper failure edge than before.

> **✅ FIXED 2026-07-25: 130 million `getenv` calls per hashmap run.**
> −13.1% on hashmap, −15.4% on stringregex, neutral elsewhere.
>
> **How it was found — and how the first attempt got it wrong.** A sampled
> `perf` profile showed ~11% of the hashmap phase in the `getenv` family. A
> `--call-graph=dwarf` profile attributed it to `invoke_on_class_shared_inner`,
> which pointed at an uncached `CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE` probe on
> the `invokevirtual` path. **That attribution was wrong** — the dwarf stacks
> only resolved ~15% of samples, and caching that flag produced *no measurable
> change* (hashmap 25444 → 25536 ms, i.e. nothing). Do not trust partial dwarf
> unwinds on this binary; it is built `debug = "line-tables-only"` with no frame
> pointers.
>
> **What actually worked** was an `LD_PRELOAD` shim that intercepts `getenv` and
> tallies calls *by variable name* — no unwinding, no sampling, exact counts.
> Over one hashmap run (10M put/get) it recorded **~130,000,000 calls, ≈13 per
> benchmark iteration**:
>
> | calls | variable |
> |---:|---|
> | 60,008,062 | `CRATONVM_DBG_LOADER_TRACE` |
> | 20,001,011 | `CRATONVM_DBG_MH_STACK` |
> | 20,001,011 | `CRATONVM_DBG_MH_ADAPTER` |
> | 20,001,005 | `CRATONVM_DBG_STACKLESS` |
> | 10,002,013 | `CRATONVM_DBG_H2TRACE` |
>
> All were uncached `std::env::var`/`var_os` probes on the `new` opcode and
> `try_stackless_invoke` paths (~40 call sites, `CRATONVM_DBG_LOADER_TRACE`
> alone had 33). `getenv` takes the process environ lock and linearly scans
> environ, so each one is far from free. Most had the env probe as the **left**
> operand of an `&&` whose right operand is a cheap string compare, so the
> `getenv` ran unconditionally and the cheap test could never short-circuit it.
>
> Fix: cached predicates in `vm/src/runtime/env_cache.rs` (`cached_is_set!` /
> `cached_is_ok!`, the idiom already used for ~80 other flags) plus local
> `OnceLock` helpers in `native-builtins`, which cannot reach the vm crate.
> Total `getenv` calls per hashmap run: **130,000,000 → ~8,000**.
>
> Measured interleaved on a quiet host, same base commit, both arms fat-LTO,
> with `arithmetic` as an unaffected control:
>
> | phase | before | after | delta |
> |---|---|---|---|
> | hashmap | 21961 ms | 19078 ms | **−13.1%** |
> | stringregex | 462 ms | 391 ms | **−15.4%** |
> | bintrees | 1988 ms | 2000 ms | ~0 |
> | arithmetic (control) | 4098 ms | 4078 ms | ~0 |
>
> This does **not** close any gate phase — hashmap is still ~4.2x over a budget
> that nothing reproduces. It is an independent, real win on the interpreter's
> native-invoke path.
>
> **Generalisable lesson:** when a profile says "time is in `getenv`" (or any
> libc leaf), an `LD_PRELOAD` counting shim identifies the culprit by *name* in
> one run and cannot be fooled by missing unwind info. Reach for it before
> trusting a call-graph attribution.

> **bintrees 1.24x: investigated, NOT the bt18 regression.** The `anchored`
> 1550 row derives from `binarytrees-half-gap-20260718.md`'s 1468 ms, and
> `bt18-inline-tlab-regression-20260724.md` verified the fix at 1527–1533 ms.
> Quiet-host measurement is ~1990–2200 ms. All of the bt18 doc's own acceptance
> criteria still hold on current dev, so the regression it describes has **not**
> recurred:
> - **single** young GC cycle under `CRATONVM_DBG_GCPHASE=1` (the doc's
>   "method of record"; the regressed state showed two), checksum `68332206`;
> - `CRATONVM_NO_JIT_INLINE_TLAB_NEW=1` → 5884 ms vs 1988 ms default (**2.96x**),
>   so the inline TLAB `new` fast path is active and carrying its weight;
> - `CRATONVM_NO_JIT_INLINE_PUTFIELD=1` → 4973 ms vs 1988 ms (**2.5x**), so the
>   inline constructor stores are being emitted — this is exactly the "regains a
>   measurable delta" check the doc asks for.
>
> Nor is it a harness-transfer artifact: the CratonBench `bintrees` phase and
> the standalone `bench/BinTreesClassic.java` the 1468 ms number came from are
> **byte-identical kernels**, and measured head-to-head on the same binary they
> agree — 2204 ms vs 2152 ms. The original harness no longer reproduces its own
> recorded number either.
>
> Also refuted: memory fragmentation / transparent huge pages. Despite the host
> showing 96% compaction failure after 3 days uptime, the running VM's heap is
> `AnonHugePages: 1912832 kB` of `Anonymous: 1914180 kB` — **99.93% huge-page
> backed** — so TLB pressure is not the mechanism.
>
> bintrees therefore joins the other three rows: in the documented fixed state,
> with the optimisations verifiably active, measuring ~1.3x its recorded number
> for reasons not yet explained by code, load, harness, heap size, or paging.

