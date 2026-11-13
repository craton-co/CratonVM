# Proposal: price compilation before admitting it (warm-up cost architecture)

> **2026-11-11 update: execution/compile cost remains OPEN.** Phase 0 now has a
> checked-in [weighted leaf-sample attribution tool](../../../tools/bench/apps/netty/README.md)
> with parser/accounting self-checks and HotSpot compilation-range support.
> Every sampled instruction period is accounted for, including unresolved code.
> The recorded frame-pointer cohort predates the latest collector-policy merge;
> it must not be treated as a current-source performance price. Fresh unlogged
> merged-source class totals remain about 6.7x HotSpot; see the
> [current report](NETTY_BYTEBUF_PERFORMANCE_GAP_ANALYSIS.md).
> The C1-first startup/javac validation debt noted below is still open.


- **Date**: 2026-11-03. **Status**: PROPOSAL, nothing built. Scopes the open half of
  `netty-buffer-suite-cpu-cost-jit-compile-20261102.md` (read it first; every number below is from that page, measured on host 64.177.43.150
  in user-mode instructions, none re-measured for this proposal).
- **Decision wanted**: approve Phase 0 only (a measurement, no behaviour change), then re-scope from its table. Phases 1-3 are sketched so
  the cost of the whole is visible, not so they start.

## Premise to attack

Every fix so far (supersede gate, C1/C2 thresholds, `SpliceScopeChain` sharing, the unswitch rescan cleanup) takes the same premise:
**the compile pipeline is right and only its admission count is wrong**. The data does not support stopping there:

* The best policy arm (`C1_THRESHOLD=3000` + `C2_THRESHOLD=100000`) is 229-254 G against HotSpot's 35.5 G, still ~6.5x, and it is the arm
  `CompilationPolicy::default` documents as leaving Hibernate/JAXB bootstraps interpreted. No threshold pair reaches the <= 2x target.
* Compile threads are ~31 % of samples. Delete compilation entirely and the class is ~210 G, still ~6x. So **compile cost alone cannot
  close the gap**; ~69 % of the instructions are elsewhere and this page has never attributed them (test threads 34 %, the other ~35 % is
  named nowhere in the doc). Any plan that only cuts compile work caps out near 6x.
* The user-visible symptom is not the ratio. It is the `*MultipleThreads` tests (`@Timeout(30)`) timing out on a loaded host. The gate that
  matters is wall time under load, which user-mode instructions only proxy.

## Phase 0: attribute the whole 309 G (the lever; do this first)

Build one script (not a profiler): `perf record -g` on a force-frame-pointers build, bucket every sample by
**thread class** (test worker, compile worker, main, GC, other) x **execution tier** (interpreter, C1 body, C2 body, runtime helper/native,
class load, GC). Run the HotSpot reference through the same buckets (`perf` on its JIT frames by thread name) and add HotSpot's compile
counts (`-XX:+PrintCompilation`) next to CratonVM's 2 328 C1 + 2 402 C2.

* Output: one table whose rows sum to 100 % (+-5 %) and whose columns are CratonVM / HotSpot / ratio.
* Exit: the table says which bucket owns the gap. Candidates: interpreted execution before/between tiers, per-call cost of compiled
  bodies (the doc's earlier 36 M invocations over 3 194 methods), runtime helpers, compile.
* Why first: the rest of this page is ordered by that table. If interpreted execution dominates, Phases 1-2 are the wrong work.
* Use `instructions:u` at host load < 8 (kernel spin locks poison time profiles above ~30), interleaved reps, medians.

## Phase 1: cost/benefit admission instead of count thresholds

Today a method is admitted when a counter crosses a constant (`CompilationPolicy`: C1 500, C2 20 000, supersede gate at the C2 bar).
The count says nothing about what the compile costs or how long the result will live. The Netty class shows the failure: 2 328 C1 bodies,
many replaced by C2 within seconds.

* **Data shape first.** One per-method ledger (Arc'd, owned by the tier manager, read by the compile worker): bytecode size, inlined
  size, measured compile cost of the last attempt (thread-CPU ns or an instruction-free proxy such as nodes lowered), invocation count at
  each publish, invocations since. No new global; per VM, like the rest of the policy.
* **Rule.** Admit tier T when `expected_remaining_invocations x (cost_now - cost_T) > compile_cost_T`, with the invocation rate taken from
  the entry counter (`entry_counter.rs`, already on) over the last window. A method whose rate predicts crossing the C2 bar within one
  compile latency goes interpreter -> C2 and skips C1; a low-rate method pays nothing it cannot recover.
* **The known counter-example stays a gate.** Reflection-heavy bootstraps (Hibernate/JAXB model construction) were interpreted when C1 was
  delayed; they are short-lived, low-rate methods, which is exactly where a rate rule must not starve them. Acceptance requires the
  application startup gauntlet (Spring Boot sample, Hibernate, JAXB, Quarkus suite) not to regress, not this class alone.
* Default-off switch until it clears every gate below; a switch that fails stays off, never deleted.

## Phase 2: spend less per compile (only if Phase 0 says compile is a large bucket)

From the compile-thread profile (30.7 % of samples; `ir_tier_attempt` 25.9 %, `optimize_with` 10.2 %, `ir_lower` 6.5 %,
`resolve_inline_site_from` 6.1 %):

1. **Persistent frame states.** `SafepointSnapshot` owns `Vec`s; unswitching and unrolling clone whole snapshots per cloned bci
   (`plan.push((si, sp.clone()))`, then `install_copy_frames` renames them again). Make slot vectors copy-on-write (shared until the
   rename touches one) so a clone is O(1). Same reasoning that made `SpliceScopeChain` an `Arc`; that landed ~3 %.
2. **Inline-site resolution memo.** `resolve_inline_site_from` re-resolves the same callee for the C1 and the C2 compile of one method.
   Cache the resolution keyed by (callee, profile epoch); invalidate on class load/redefinition. Must be proved correct against CHA and
   retransform (Mockito flushes the cache; see its known issue).
3. **One lowered graph, two pipelines.** If the graph builder runs from bytecode for both the C1 and the C2 compile, build the base graph
   once and let each tier clone it. **Unverified**: confirm it runs twice before costing this.
4. **Compile budget per method.** A cap on inline-site expansions per compile, so an 11 KB body cannot spend 10x the median.

Each is independent; each ships behind its own switch with a counter proving engagement before any A/B.

## Phase 3: cross-run reuse (opt-in, last)

`profile_store.rs` already saves/loads profiles (`CRATONVM_JIT_PROFILE_SAVE/LOAD`). Extend it with the *compile plan*: which methods reached
which final tier. A second run admits those methods straight to their final tier on first admission, skipping the intermediate C1 and the
warm-up count. This helps repeated suite runs and CI; it does **nothing** for the first cold run, so it is not a fix for this page, and it
adds a stale-plan hazard (a plan from another build of the same jar) that needs a content hash on the class bytes.

## Gates, applied to every phase

* Engagement first: a census (counter or env trace) that the new path fired, before any A/B.
* `instructions:u`, base binary vs new binary interleaved, >= 4 reps, host load < 8, medians; both HotSpot and `--real-jdk`/`--compatible`
  checked, since a fix for one mode has broken the other before.
* This class, CratonBench fib / sieve / matrix / bintrees / hashmap / stringregex (no kernel may execute more instructions), `javac` of 2
  and of 120 Netty sources, the application startup gauntlet, and the `tiered::` unit tests (219 today). Chunk `cratonvm-jit --lib` on
  Windows: four tests abort the run on dev itself.
* A flip to default-on is its own commit with a `WARNING:` subject and a greppable site marker.

## Cost and expected outcome (estimates, not measurements)

| phase | effort | what it can move | bound |
| --- | --- | --- | --- |
| 0 attribution | 1-2 days | nothing; picks the rest | n/a |
| 1 admission model | 1-2 weeks, plus the gauntlet | compile + wasted C1 share | best policy arm so far ~6.5x; a model can only approach it |
| 2 per-compile cost | 3-4 weeks, items independent | the 30 % compile share | ~3 % per item measured so far (snapshot sharing) |
| 3 plan reuse | 1 week | second-run warm-up only | zero for a cold run |

Phases 1-3 together are bounded by the compile share (~31 %), so **they cannot reach the old <= 2x target**; reaching it needs the
execution-side buckets Phase 0 will name, which is a separate proposal.

## Recommended re-scope

Retarget this page's exit from "<= 2x HotSpot instructions" to the symptom that fails: **no `*MultipleThreads` timeout in N consecutive
runs at host load 30-50**, plus a staged ratio target (e.g. <= 4x) set after the Phase 0 table. Retire the page when that holds; file
the execution-side gap (whatever Phase 0 shows) as its own page.

## Non-goals

A new compiler tier, a different register allocator, AOT of the JDK, or changing HotSpot-visible behaviour (both modes target HotSpot).

## Phase 0 results, 2026-11-03

Host 64.177.43.150, load 5-8, `perf record -e instructions:u` (one sample per 10 M instructions for CratonVM, 1 M for HotSpot, so a
row's error is about +-0.5 point), `AdvancedLeakAwareCompositeByteBufTest` (508 found, 499 ok, 9 aborted on every arm). Binary:
`/root/CVM/target/release/cratonvm` (0.3.0, built 2026-11-03 02:10 from another session's checkout of dev; its exact sha was not recorded,
so treat the absolute numbers as "dev of that morning"). Netty `4.2.20.Final-SNAPSHOT` from `/root/CVM/apps/netty`, classpath assembled by hand
(`/root/p0/cp.txt`); HotSpot 25 on that classpath reproduces the page's 33.6-38.0 G. `perf stat` totals: jdk-only 323 / 408 G,
`--compatible` 262 / 283 G, HotSpot 29.8-38.0 G. The lever is `tools/instr-buckets/instr_buckets.py` (thread class x module; sym rows with `--syms N`).

| thread class | HotSpot 25 | CratonVM `--compatible` | CratonVM `--real-jdk` |
| --- | ---: | ---: | ---: |
| total (sampled) | 35.3 G | 253 G (7.2x) | 426 G (12x) |
| compile threads | 22.8 G (64 %) | 116 G (46 %) | 189 G (44 %) |
| test workers | 10.2 G (29 %) | 116 G (46 %) | 221 G (52 %) |
| main + other + GC | 2.3 G (7 %) | 21 G (8 %) | 16 G (4 %) |

Inside the test workers (share of ALL samples): CratonVM `--compatible` `cratonvm_vm::runtime` (interpreter loop, dispatch, jit bridge)
17.1 % = 43 G, compiled code (perf-map + unmapped) 6.0 % = 15 G; `--real-jdk` runtime 20.0 % = 85 G, compiled code 4.9 % = 21 G.
HotSpot's compiled code is 23-26 % = 8-9 G.

### What the table says

1. **The premise holds, and the gap is not mostly compile.** Against HotSpot, `--compatible` spends +93 G on compile and +125 G elsewhere
   (57 % of the gap); `--real-jdk` +166 G on compile and +225 G elsewhere (58 %). Removing every compile instruction would leave 3.9x
   (`--compatible`) and 6.7x (`--real-jdk`). The earlier "31 % compile" was a time share; by instructions it is 44-46 %.
2. **HotSpot's own compile is 22.8 G** (64 % of its run): the class is a compile-bound workload for it too. CratonVM compiles 5-8x as many
   instructions for it, so the compile target is "within 2x of 22.8 G", not "near zero".
3. **The test threads run mostly outside compiled code.** Compiled bodies execute only 15-21 G (about 2x HotSpot's 8-9 G); the other ~100-200 G
   is the interpreter and its runtime helpers. Whether that is time spent interpreting before a body is published (compile latency feeding
   the interpreter) or bridge/helper cost on every call from compiled code is NOT separable from this table: `cratonvm_vm::runtime` mixes
   `execute_frame_from_index_loop` (3.2 % of all, the interpreter loop), `dispatch_virtual::execute_invokevirtual_cached_probed` (2.3 %),
   `jit_bridge::run_jit_body_raw` and `execute_jit_call_decoded`.
4. **Cheap compile-thread findings, each a candidate before Phase 1** (`--compatible`, share of ALL samples; `--real-jdk` in brackets):
   `RandomState::hash_one::<&TypeId>` 2.84 % = 7 G plus SipHash `write` 1.20 % [std::hash module 2.3 %]; `VmFlags::declared_ref` 2.19 % = 5.5 G
   [`cratonvm_types::flags` 2.9 %]; `ir_verify::verify_graph_inner` 1.83 % = 4.6 G; `home_reload_targets_except` 1.24 %. Together about 8 % of the
   total, 17 % of the compile threads. The callers of the `TypeId` hash and the flag reads were NOT identified (a `--call-graph dwarf` recording
   produced no resolvable chains on this release binary); a frame-pointer build ([[cratonvm-flat-profile-needs-frame-pointers]] recipe) is needed
   for that. Whether `verify_graph_inner` runs in release by default is unchecked.

### Phase 0b results, 2026-11-03 (same host, binary and class; `--compatible`; the class repeated as extra argv, one JVM)

`CratonRunner` runs every argv class name in turn, so `C C C C` is four passes in one process. `I(4) - I(1)` over three is the cost of a WARM pass.

| | pass 1 | warm pass (avg of 2-4) | warm / HotSpot warm |
| --- | ---: | ---: | ---: |
| HotSpot 25, instructions:u | 38.6 G, 1.43 s | 7.8 G, 0.2 s | 1x |
| CratonVM `--compatible`, instructions:u | 287 G, 10.1 s | 68.7 G, 2.0-2.4 s | **8.8x** |

1. **It is not (only) warm-up.** The first pass is 7.4x HotSpot, the warm pass 8.8x: the ratio does not improve once everything has run. A
   warm pass is 24 % of CratonVM's first pass and 20 % of HotSpot's, so the cold excess is proportional too. Phase 1 (admission) can shrink
   only the cold excess (218 G against HotSpot's 31 G) and none of the 69 G a warm pass keeps costing.
2. **Compile threads never go quiet.** Per warm pass (sampled, N=4 minus N=1, over 3): compile 18.8 G, test workers 46.9 G, main 7.1 G
   (sum 72.8 G, matching the 68.7 G `perf stat`). `jit-method-stats` over the same runs: per warm pass about 176 C1 + 364 C2 + 9 OSR
   compiles, 93 deopts, 430 newly tracked methods and about 7 s of compile-thread time, against 955 C1 / 2 718 C2 / 91 OSR / 146 deopts
   in pass 1 (code cache: 940 recompilations over 4 561 methods). So deopt-driven recompilation and a steady flow of new methods keep
   26 % of a warm pass in the compiler. **This is the lever Phase 1 did not name**: a ledger needs a recompile-churn term (why does a method
   deopt 93 times per pass; do the new methods come from LambdaForms, proxies or generated test classes).
3. **Compiled code itself is close to HotSpot's.** Compiled-body instructions: about 12 G per warm pass (perf-map + unmapped,
   N=4 minus N=1) against HotSpot's ~8-9 G for a whole pass. Not the gap.
4. **What is in the other ~35 G of test-thread work per warm pass:** `cratonvm_vm::runtime::interpreter` 14.1 % of the N=4 run (interpreter loop,
   `dispatch_virtual`, `jit_bridge`, `record_call_for_frame`), `jit::helpers` 3.3 %, GC (`vm_heap` + `zgc`) 4.2 %, `ProfileStore` 1.5 %,
   `lang_invoke::member_name` 1.4 %, `conservative_roots` 1.0 %. `dispatch_virtual` (4.0 % of pass 1) costs more than the interpreter loop
   proper (2.8 %), and the call-boundary code (`jit_bridge` 2.7 %, `record_call_for_frame`, `ProfileStore::get_or_insert_borrowed` 1.2 %)
   is as large as the bytecode interpretation: much of the interpreted cost is crossing between interpreter and compiled code, not
   interpreting.
5. **Interpreted share.** `interp-frames`: pass 1 runs 5.8 M of 33.3 M invocations interpreted (17 %, 15 915 methods); a warm pass runs about
   1.1 M of 13.1 M (8 %). The top rows are tiny netty getters (`EmptyByteBuf.getBytes`, `UnpooledSlicedByteBuf._getLong`,
   `AbstractByteBuf.checkIndex`) that are interpreted hundreds of thousands of times. `tierup-decline` marks 5.4 M of them "admitted" (the policy
   did not bar them; only `registered_native` rows such as `Unsafe.putLongUnaligned` are declined), so they are interpreted for want of a
   published body, not because policy refuses them. The census does not say why the body is missing (queued, superseded or deopted
   and re-queued): that needs the per-method publish timeline, not another aggregate.

### What changes in the proposal

* **Phase 1 stays, but is smaller than sold**: it targets the 218 G cold excess and the recompile churn; it cannot touch the 69 G warm pass.
  Add the churn term (deopts per method, recompiles per method, new-method flow) to the ledger before the cost/benefit rule.
* **New Phase 1a (first, cheapest): find what deopts 93 times and what keeps minting methods per pass.** `jit-method-stats` already prints
  deopt and recompile tops; a per-pass diff names them. A fix here pays in every pass, cold and warm, and can be priced with the same
  `C C C C` lever.
* **New Phase 2b, execution side: the interpreter/compiled call boundary** (`dispatch_virtual`, `jit_bridge`, `record_call_for_frame`, profile
  writes on the call path) and the interpreted tiny getters. This is a separate proposal; the warm-pass lever gives it its measure.
* **Re-scope the exit** from the earlier recommendation: the gate is the warm pass (target stated as a ratio against HotSpot's 7.8 G) next to the timeouts.
* Limits of 0b: one recording per arm; the interpreter/dispatch split uses symbol prefixes, so inlined callees land in the caller's row;
  `--real-jdk` was not repeated (it was 1.7x `--compatible` on pass 1); the binary's sha is unrecorded.

## Phase 1a results, 2026-11-03 (what deopts and what keeps compiling; same host, binary, class, `--compatible`, four passes in one JVM)

Method: `CRATONVM_DBG=deopt` (event lines `<method> reason= bci= action=`) and `CRATONVM_DBG=jit-compiled` (one `put <method>` per published
body), split into passes at the `@@RESULT` lines; HotSpot 25 compile rows from `-XX:+PrintCompilation` on the same class and classpath.
Scripts were ad hoc (`/root/p0/an.py`, `an2.py`, removed); the numbers below are what they printed.

| | pass 1 | pass 2 | pass 3 | pass 4 |
| --- | ---: | ---: | ---: | ---: |
| CratonVM compile publishes | 5 707 | 809 | 537 | 503 |
| of which first-time methods | 4 632 | 600 | 362 | 318 |
| of which recompiles | 1 075 | 209 | 175 | 185 |
| CratonVM deopt events | 205 | 91 | 96 | 67 |
| HotSpot compile rows (1 pass: 5 846; 4 passes: 7 545) | 5 846 | warm avg 566 | | |

1. **The compile COUNT matches HotSpot.** 5 707 publishes against HotSpot's 5 846 compile rows in pass 1, and 616 against 566 per warm pass. Admission
   is not creating extra compiles in aggregate, so a stricter admission rule (the Phase 1 premise) has little to remove. Cost per compile does:
   119 G / 5 707 = about 21 M instructions per CratonVM compile (30 M in a warm pass: 18.8 G / 616) against HotSpot's 22.8 G / 5 846 = 3.9 M. That is **5x
   per compile** and is the whole compile column of Phase 0 (116 G against 22.8 G). Phase 2 (cost per compile) is the compile lever, not Phase 1.
2. **Warm-pass compiles are the long tail, not churn.** 60-75 % of a warm pass's publishes are methods compiled for the FIRST time (JUnit
   `org/junit/jupiter` and `platform`, `io/netty/buffer` lukewarm methods, `java/util/stream`): methods under ~170 calls per pass cross the
   500-call C1 bar in pass 4. HotSpot has the same tail (566 compiles per warm pass). Pass 1 also pays ~1 700 compiles in ByteBuddy
   (`net/bytebuddy/description` 542, `dynamic` 185), `jdk/ 446 and `java/lang/invoke` 363: Mockito class generation, once.
3. **Deopts are a broad shallow tail, not one stuck method.** About 85 events per warm pass over 36-60 methods; 107 distinct methods deopt in
   passes 2-4 and only 17 of them also did in pass 1. `ClassLoading` deopts fall 36 -> 5 -> 3 per pass (the top single row,
   `AbstractByteBufTest$TestGatheringByteChannel.<init>`, 19 events, is a class-hierarchy assumption broken by a class loaded per pass); `UncommonTrap` /
   `RecompileAndReinterpret` stays 23-42 per pass (`TimSort.countRunAndMakeAscending`, `CollectionUtils.forEachInReverseOrder` 13, netty
   `AbstractByteBuf.ensureWritable` / `duplicate` / `checkDstIndex`). Deopts do not explain the warm-pass compile column: 85 events cannot make
   500-800 compiles.
4. **Tier mix is the open anomaly.** End-of-run census (pass 1): 955 C1 + 2 718 C2 compiles, and 3 310 of 4 381 compiled methods sit at tier `c2`;
   HotSpot's mix is 4 645 C1-tier compiles and 925 C2 (16 % at the top tier). If CratonVM's `c2` bodies cost far more than `c1` ones, the C2-class count
   (2.9x HotSpot's) is where the 5x per-compile cost comes from. Not established: the census was not split by tier, and "tier" here may label IR bodies of
   both kinds; read `jit/src/tiered.rs` and split `total_compile_time_ms` by tier first.

### What this does to the plan

* **Phase 1 (admission ledger) is demoted**: compile counts already match HotSpot's, so its upside on the compile column is small. Keep only the
  cold-excess piece if Phase 2 leaves one. The churn term added after 0b is answered: there is no churn problem, there is a per-compile cost problem.
* **Phase 2 becomes the main compile lever, in this order**: (a) 1b: split compile time by tier and find why 75 % of methods reach the top tier;
  (b) the compile-thread costs Phase 0 found (`TypeId` hashing with the default hasher 2.8 % + 1.2 % SipHash, `VmFlags::declared_ref` 2.2 %,
  `verify_graph_inner` 1.8 % of ALL instructions, together about 8 %); (c) snapshot sharing, inline-site memo, per-compile budget as before.
* **Phase 2b (execution side) is unchanged and bigger**: warm-pass compile is 19 G of 69 G; the other 50 G is test-thread work.
* Limits: counts compare CratonVM "published bodies" with HotSpot "compile rows" (OSR rows included in both, tiers 1-3 counted as C1), so they agree to
  about 10 %, not exactly; one run per arm; deopt reasons are as logged, not interpreted.

## Phase 1b results, 2026-11-03 (compile cost by lane, and where the top-tier compiles come from; same host, binary, class, `--compatible`, one pass)

**Lane split without a code change.** The compile lanes are separate worker pools (`compiler_thread_counts`: 1 C1 worker spawned first, then
`log2(cpus)` = 4 C2 workers; `jit/src/tiered.rs` ~8188), so per-thread `instructions:u` from `perf script -F comm,tid` splits them. The mapping of the
lowest tid to the C1 lane is inferred from spawn order and from the counts (1 011 C1 compiles on a 4.8 G thread), not read from a thread name.
HotSpot splits by comm (`C1 CompilerThre` / `C2 CompilerThre`).

| lane | CratonVM compiles | CratonVM instructions | per compile | HotSpot compiles | HotSpot instructions | per compile |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| C1 | 1 011 | 4.8 G | 4.7 M | 4 645 | 3.3 G | 0.7 M |
| C2 (incl. 91 OSR) | 2 797 | 132.2 G | 47 M | 925 | 18.6 G | 20 M |
| total | 3 808 | 137 G | | 5 570 | 21.9 G | |

1. **The whole compile gap is the C2 lane**: 132 G of 137 G. CratonVM's C1 lane costs 1.5x HotSpot's. The C2 lane is 7.1x, which is **3.0x as many
   top-tier compiles times 2.4x the cost of each**. The 1a "counts match HotSpot" holds only in aggregate: HotSpot does 83 % of its compiles at
   C1 (tiers 1-3), CratonVM 27 %. So admission does matter, for the top tier only; Phase 1's demotion in 1a is partly reversed.
2. **Why 75 % of methods end at the top tier.** In this binary `tier` is `body.used_ir_backend` (`note_callee_publish_to_tiered_manager`,
   `jit_bridge.rs` ~33800): a body published by the IR backend IS "C2". The invocation-count policy admits few of them (`CRATONVM_DBG=tier-enqueue`:
   951 C1 and only 197 C2 enqueues). The rest enter through the **callee door**: when compiled code needs an uncompiled callee,
   `defer_callee_compile_to_worker(.., optimize=true)` queues an optimizing compile (`CRATONVM_DBG=jitc`: 1 815 `callee-door DEFERRED optimize=true queued=true`,
   389 `queued=false`, none with `optimize=false`) on the first miss, with **no hotness bar** (the 20 000 bar of `request_c2_supersede` guards only the C1-publish door).
   Of 2 479 `c2-supersede published` lines, 1 839 (74 %) say `c1=none`: the method's first body is a top-tier one, which is the 75 % in the census.
3. **Sized upper bound, not a result**: if the ~1 815 door compiles cost a C1 compile (4.7 M) instead of a C2 one (47 M), the saving is up to
   1 815 x 42 M = 77 G, 30 % of this run's 253 G, taking the C2 lane to ~2.3x HotSpot's. It overstates: callees that turn hot are still promoted
   (their C2 compile then happens later, at the bar), the C1 compiles are extra work, and slower interim code costs instructions. Nobody has measured it.
4. `jitc` produced 237 MB for one pass and `callee-guard`/`inline-resolve REFUSED` lines at 354 k / 196 k: not usable as a census without filtering.

### What this does to the plan

* **Phase 2, first item (replaces "the 5x per compile is generic")**: gate the callee door's optimizing deferral on hotness. When a callee has no body and its
  count is below `c2_hotness_bar`, defer at C1 (or not at all) and let the entry-counter drain promote it, the way the C1-publish door already does. Where:
  `callee_door_defers_now` / `defer_callee_compile_to_worker` (`jit_bridge.rs` ~3149-3210, caller ~30402). Priced with the same lever as the supersede gate:
  `instructions:u`, base vs gate, this class + the six CratonBench kernels + `javac` + the startup gauntlet (a callee that stays at C1 longer can slow a
  tight loop that calls it; that is the risk to measure).
* The remaining per-C2-compile cost (47 M against HotSpot's 20 M) is the generic pipeline cost of Phase 2 (hash, flags, `verify_graph_inner`, snapshot clones).
* Not done: the A/B itself (needs a patched binary built on the host; this host has no GitHub access, so ship the patch over the pipe), a per-lane split of the warm
  passes, and the 389 `queued=false` refusals.

### Phase 2a results, 2026-11-03: the callee door defers at C1 (`CRATONVM_JIT_CALLEE_DOOR_C1_FIRST`, flipped default ON the same day; `=0` restores)

The patch is one line of policy: the door's deferred task is `target_tier = C1` even when the door would have optimized
(`callee_door_deferred_optimize`, `jit_bridge.rs` next to `callee_door_defers_now`). It is a switch, not a hotness bar: the entry counters already
promote a hot C1 body at `c2_threshold`, so "defer at C1" is the bar. One binary (no-LTO release, host 64.177.43.150, load 1-2), env var set vs
unset, interleaved, `perf stat -e instructions:u`, `--compatible` unless noted. Netty class: every run `ok=499 failed=0 aborted=9`.

| row | base | gate | change |
| --- | ---: | ---: | ---: |
| Netty class, 1 pass, `--compatible` (3 reps) | 292 / 268 / 299 G | 230 / 239 / 264 G | median **-18 %** |
| Netty class, 1 pass, default `jdk-only` (2 reps) | 385 / 456 G | 372 / 367 G | -3 % to -19 % |
| Netty class, 4 passes in one JVM (2 reps) | 530 / 508 G | 474 / 472 G | **-9 %** |
| `javac` (CratonBenchParam.java, 3 reps; same 2 class files, same md5) | 58.9 / 59.4 / 58.2 G | 30.2 / 29.9 / 29.8 G | **-49 %** |
| CratonBench arithmetic / fib / sieve / matrix / hashmap | identical to 0.01 % | | 0 |
| CratonBench btrees (3 reps) | 55.9 / 55.1 / 55.8 G | 54.8 / 56.1 / 54.4 G | noise (wall 1.38-1.42 s both) |
| CratonBench stringregex (3 reps) | 4.77 / 4.77 / 4.67 G | 4.44 / 4.29 / 4.32 G | -8 % |

* No kernel got slower: the wall times of all seven are equal within 1-2 ms, which was the risk named in 1b (a callee held at C1 slowing a tight loop).
* Netty's spread (+-7 % per arm) is the compile threads' timing; the direction held in all 7 pairs of Netty rows. The `javac` and kernel rows are tight.
* The gain is smaller than the 30 % upper bound for the class and larger for `javac`, which is compile-dominated.
* Not measured: wall time (host load made it noisy; the switch is judged on instructions), the startup gauntlet, Spring Boot and the other real workloads, `--compatible`
  vs `jdk-only` behavior beyond the counts above, and a hotness bar between "C1 first" and "optimize at the door" (e.g. only defer at C2 for an already-counted callee).
  The user chose to flip it ON before the gauntlet and the real-workload suites ran (`WARNING: callee-door-c1-first-default-flip` commit); those runs are the open check, and `=0` restores the old behavior.
* Tests: `the_c1_first_switch_demotes_the_deferred_tier` (pure decision), plus the flag-surface listing. Engagement is shown by the A/B itself (javac -49 %).

## 2026-11-12 validation residual

The explicit 120-source control found a reproducible foundational-mode javac
error when C1-first is disabled, including on the previously delivered runtime.
The enabled policy and compatible controls match all 225 HotSpot class files.
See [the reproduction and remaining controls](netty-javac-120-c1-door-off-loses-resource-symbol-20261112.md).
Startup and smaller javac diagnostics do not retire this remaining correctness check.
