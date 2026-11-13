# Netty `io.netty.buffer` suite costs ~9x HotSpot's CPU: warm-up of a 500-test class

> **2026-11-12 verification: OPEN.** On merged runtime `cbe3f7b297`
> (upstream `5427879a33` included), four fresh processes per VM pass all
> 499 tests with 9 aborts and no failures. Median user instructions are
> 225.38 G foundational / 224.87 G compatible / 34.59 G HotSpot 25.
> The <=2x gate remains unmet. This is shared-host instruction evidence;
> current access-path repairs and validation are in the
> [ByteBuf report](NETTY_BYTEBUF_PERFORMANCE_GAP_ANALYSIS.md).
> The older measurements and policy experiments below remain historical.


- **Date**: 2026-11-02 (narrowed from the 2026-11-01 page, whose `Throwable`-capture half is fixed:
  ; the C1->C2 supersede
  gate landed the same day, see "Landed" below).
- **Status**: OPEN, throughput only. With the host quiet every class passes; on a host at load >= 30 (or four of these processes at once)
  the `*MultipleThreads` tests (`@Timeout(30)`) can still time out, on base and on the fixes alike (the supersede gate made it rarer:
  0 timeouts in 3 runs at load 25-50 with the equivalent `CRATONVM_TIER_C2_THRESHOLD=20000`, against 5 / 4 / 0 for the default then).
- **Follows**:  (the per-read `Unsafe` dispatch,
  fixed) and `.../adaptivebytebufallocator-contention-hang-NOT-A-HANG-20261101.md` (the "hang", refuted).

## Measurements (host 64.177.43.150, 16 cores, shared; load 10-16 for the table below)

`AdvancedLeakAwareCompositeByteBufTest` (499 tests, JUnit parallel mode, 16 workers), one process, three runs each:

| | wall | CPU (user+sys) | user-mode instructions |
| --- | ---: | ---: | ---: |
| HotSpot 25 | 1.5-1.8 s | 10.3-11.1 s | 33-35 G |
| CratonVM before the supersede gate | 20.7-27.3 s | 107-127 s | 391-473 G |
| CratonVM after (dev + `request_c2_supersede` gate) | 19.1-20.5 s | 85-92 s | 309-329 G |

Use **user-mode instructions** (`perf stat -e instructions:u`) on this host, not wall or CPU time: other sessions drive the load to
50-100 and the kernel's memory-reclaim spin locks (`native_queued_spin_lock_slowpath`, `shrink_slab_memcg`) land in whichever thread
`perf` samples, so a time-based profile at that load is noise (a `perf record` at load 50 attributed 94 % of the samples to the kernel).

Where it goes (quiet host, earlier profile): 31 % of samples are the `cratonvm-jit-compiler` threads (`ir_tier_attempt` 24 %: lowering 8.5 %,
`resolve_inline_site_from` 6.5 %, `ir_optimize` 5 %), 34 % the test threads; the profile is flat, no function above ~1 %. Census
(`CRATONVM_DBG_JIT_METHOD_STATS=1`, load ~35): 36 M invocations over 3 194 methods; before the gate 2 529 C1 + 4 010 C2 compiles
(`total_compile_time_ms=365405`), after 2 328 C1 + 2 402 C2 (`238156`).

## Landed (2026-11-02): the C1->C2 supersede asks the C2 hotness bar

`CompilerCore::request_c2_supersede` (`jit/src/tiered.rs`), called by the compile worker after a C1 publish, refuses a method whose
count is below `c2_hotness_bar` (20 000 by default). The door used to ask for nothing: **every** C1 publish queued a C2 recompile,
so 80 % of the C1 bodies of this class were replaced by a C2 body, cold methods included. A published C1 body counts its entries
(`entry_counter.rs`, default ON) and the drain promotes a method that gets hot at the same bar, so the open door only bought C2 compiles
of methods that never got hot. The retry doors and the opt-in outside-manager door are unchanged (a first attempt at gating them too
broke 17 `tiered` tests that drive them on cold counts).

Priced as the code comment on `c2_upgrade_min_invocations` asked (user-mode instructions, base binary vs gate binary, load 8-17):

| workload | base | gate |
| --- | ---: | ---: |
| Netty class (above) | 391-473 G | 309-329 G |
| CratonBench fib / sieve / matrix / bintrees (2 runs) | 16.4-16.5 / 26.8 / 22.5-22.6 / 18.8 G | 16.1-16.2 / 26.5-26.6 / 22.3-22.4 / 18.5 G |
| CratonBench hashmap / stringregex | 45.1 (one run 69.2) / 4.3-4.4 G | 44.6 / 3.3-3.4 G |
| `javac` of 2 files | 175 / 191 G | 142 / 141 G |
| `javac` of 120 Netty sources | 4 133 / 4 035 G | 3 694 / 3 997 G |

No kernel, startup or mid-size run executed more instructions with the gate (the six kernels' checksums are unchanged). Tests:
`tiered::tests::a_c1_publish_below_the_hotness_bar_queues_no_c2_supersede` (red with the gate disabled, green with it) and the 219 `tiered::`
unit tests. The 30 `vm/tests/jit_*` / `osr_*` / `tier1_tests` end-to-end binaries that do not pin `CRATONVM_TIER_C2_THRESHOLD` pass except
`jit_inherited_callee_constant_pool::inherited_callee_is_not_bail_listed_through_subclass`, which fails the same way with the gate
disabled (dev `3d00b926ac`, `compiled.is_some()` at line 190) and is not this change's. The whole `cratonvm-jit --lib` run aborts on a
crash; its 7 listed failures (`deopt::gkr_rerun_window_tests`, `direct_helpers::r21_updaters_*`, `ir::nio21w4_l2_*`, `ir_lower::*`) also
fail on the unchanged `tiered.rs`. Not done: a kill switch for the old behaviour (A/B with a binary from before `request_c2_supersede`).

## What is still open

The class is still ~9x HotSpot's CPU and instructions, so the old target (<= 2x) is not met and this page stays. The remaining cost is
warm-up, not a single function:

* **Admit later.** `CRATONVM_TIER_C1_THRESHOLD=3000` was the best single arm (351 G against 464 G in the loaded scan that also showed the gate
  at 385 G), but `CompilationPolicy::default` records that delaying C1 left reflection-heavy bootstraps (Hibernate / JAXB model construction)
  interpreted. It needs the application startup gauntlet, not this class. Stacking it on the gate is untested.
* **Spend less per compile.** A C1 body that a C2 body supersedes within seconds; lowering and inline-site resolution on 11 KB bodies (the
  profile above). Target when it closes: the HotSpot ratio measured in user-mode instructions on this class.

## Policy-knob scan, 2026-11-02 (host 64.177.43.150, dev `b518666b28`, no-LTO release, load 7-16, `instructions:u`, 2-3 reps per arm)

Same class, `perf stat -e instructions:u`, one process per run, every run `found=508 ok=499 failed=0 aborted=9`. HotSpot 25: 35.5 G.

| arm (env on the dev binary) | user-mode instructions | vs HotSpot |
| --- | ---: | ---: |
| default | 346 / 360 / 389 G | ~10x |
| `CRATONVM_TIER_C2_THRESHOLD=100000` | 361 / 338 G | ~10x |
| `CRATONVM_TIER_C1_THRESHOLD=3000` | 289 / 297 / 295 G | ~8x |
| `CRATONVM_TIER_C1_THRESHOLD=10000` | 285 / 283 G | ~8x |
| `C1_THRESHOLD=3000` + `CRATONVM_JIT_THRESHOLD=3000` | 338 / 300 G | ~9x |
| `C1_THRESHOLD=3000` + `C2_THRESHOLD=100000` | 229 / 254 G | ~6.5-7x |
| `CRATONVM_TIER_C2_THREADS=1` | 479 / 445 G | ~13x (worse) |

Admitting C1 later and C2 much later together is the best arm and still ~6.5x: no tiering policy reaches the <= 2x target; the rest is per-compile
cost and per-call cost (the profile above). The best arm is also the one `CompilationPolicy::default` documents as leaving reflection-heavy
bootstraps (Hibernate / JAXB model construction) interpreted, so it was **not** made a default: it needs the application startup gauntlet first.
Nothing was changed in code; the page stays open for the compile-cost work.

## Compile-thread profile, 2026-11-02 (frame-pointer build, `perf record -g`, dev `b518666b28`)

Compile threads are 30.7 % of all samples. Inclusive, of all samples: `ir_tier_attempt` 25.9 %, `optimize_with` 10.2 %
(`class_unswitch::unswitch_bimorphic_class_loops` 6.7 % of it), `ir_lower` 6.5 %, `resolve_inline_site_from` 6.1 % (about a fifth of the
compile thread, not the 28 % quoted above). The single biggest self cost was `Vec<SpliceScopeFrame>::clone`, 2.7 %, almost all under
`unswitch_one_loop`'s `SafepointSnapshot` clones (each frame owns a `String`). Landed: `SpliceScopeChain` is shared (`Arc`) between
cloned frame states. The `instructions:u` A/B (4 reps, interleaved) is inside the noise (base 324-415 G, new 316-388 G); the profile
share says about 3 %. Still open: unswitching clones whole snapshots per cloned bci (6.7 %), `ir_lower`, and the per-call costs above.

## Unswitch pass cleanup, 2026-11-03 (not measured)

`unswitch_one_loop` (`jit/src/ir_optimize/class_unswitch.rs`) rescanned every safepoint snapshot once per escaping value and called
`subst.values().any(..)` once per `in_loop` query. It now builds the value set once and rewrites the snapshots in one pass over a
`v -> exit_phi` map (same result: exit phis are fresh nodes, so a rewritten slot never matches a later key). It compiles and the
`cratonvm-jit` lib builds; there is no unit test that reaches `unswitch_bimorphic_class_loops` and no `instructions:u` A/B on the Linux host
was run, so the gain is unpriced. The page stays open: the class is still ~6.5-10x HotSpot, and the per-bci snapshot clone in the plan
(`plan.push((si, sp.clone()))`), `ir_lower` and `resolve_inline_site_from` are untouched.

Architecture proposal for the open half: `netty-buffer-suite-compile-cost-architecture-proposal-20261103.md`.
