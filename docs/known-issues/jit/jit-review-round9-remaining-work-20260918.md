# JIT review round 9: what is left to do

**Status:** OPEN (index). Written by the integrator at the close of round 9 (waves 1-11,
2026-09-18 .. 2026-09-19), when the branch `fix/jit-review-r9-20260918` was merged into `dev`.
Every item below is either an open page in this directory (the page holds the evidence, the
numbers and the next step) or a standalone item written out here in full because it has no
page of its own.

Measure A/B interleaved against a previous binary in the same session: this machine's speed
drifts between hours. The probes are in `C:\craton\jitr9-probes\` (harness: `run.sh <exe> <label>`;
per-wave A/B scripts: `ab10.sh`, `ab11.sh`).

## 1. Correctness (fix first)

1. **`chm-segment-resize-holds-the-stripe-write-lock-across-an-allocation-20260918.md`.**
   A suspected GC deadlock, found by reading the code and not reproduced. Write the reproducer
   first.
3. **The wave-10 adversarial review was never completed.** Lane `review11` was stopped before
   it filed anything. Nobody has reviewed the wave-10 and wave-11 changes for wrong code
   beyond the probes, the unit tests and the 95-vector regression suite. The checklist to run,
   with Java reproducers against HotSpot:
   - **Private instance self-call** (`CRATONVM_JIT_INSTANCE_SELF_CALL`, `jit/src/lib.rs`,
     `x64/op_invoke.rs`): null receiver, a subclass receiver of a private method, nestmates,
     `synchronized` methods, deep recursion and `StackOverflowError`.
   - **Retire cell, now default ON** (`CRATONVM_JIT_RETIRE_CELL`), including context-free
     callees: eviction races and OSR frames.
   - **`cached_osr_body_is_despec_stale`**, and wave 11's `osr_guard_exit_reason` and
     `guard_reason_for_trapped_frame`, which now charge OSR guard exits as deopts.
   - **Wave 11's OSR re-entry memo** (`CRATONVM_JIT_OSR_REENTRY_MEMO`): a class redefinition
     or code-cache flush between entries, and multiple VMs on one thread.
   - **The IR box/unbox fold** (`ea_ir_bridge.rs` / `ir.rs`): box identity under `==`, null,
     the `Integer.valueOf` cache range, and a box that escapes after `intValue()`.
   - **The division-guard re-tier** (`vm/src/jit/helpers.rs`).
   - **The stackless door `initialisation_is_unobservable`** (`vm/src/runtime/exceptions.rs`).
   - **The ZGC pristine-chunk skip** (default ON) and the lazy TLAB zeroing (default OFF):
     any path that writes heap memory without going through the arena doors.
   - **collect11's lock-free per-VM `Integer` cache table**
     (`native-builtins/src/lang_math.rs`): multiple VMs, and GC remap of the leaked tables.
   - **chm11's `ConcurrentHashMap` wrapper-key chain walk** (`jit_chm_get`) under a concurrent
     resize.
   - **The SpliceCast re-probe in `direct_callee_lookup`**: binding to a body a concurrent
     door published.

## 2. Performance (open pages)

| page | where it stands (w11, interleaved) | next step |
|---|---|---|
| `../perf/perf-implicit-exceptions-in-compiled-code-cost-microseconds-each-20260918.md` | `ExcLoop` 3.0-3.3 us per caught exception (HotSpot 0.4-0.7) | Run the handler in the compiled body (JIT exception-dispatch codegen in `jit/src/x64/*`). The IR zero-divisor guard should throw directly, but only after section 1 item 1. The fast-throw default is a product decision, see section 3. |
| `../perf/perf-concurrenthashmap-get-native-override-is-360x-slower-than-hotspot-20260918.md` | `VolSplice chm` 17.6 s for 30 M gets (w10 32.4 s; HotSpot ~0.1 s) | Remove the generic native call per `get`: a JIT leaf call or an intrinsic, and the loop's `Integer` boxing. |
| `../perf/perf-collection-natives-cost-a-generic-native-dispatch-per-call-20260918.md` | `BoxProbe` box/put/get 765 / 1 230 / 665 ms (HotSpot 26 / 15 / 11) | Inline `Integer.valueOf` allocation in compiled code. The design spans `op_invoke.rs`, `objects.rs`, `ir_lower.rs` and `runtime_lowering.rs`. Also drop the needless `Object[]` store check (about 9.6 % of `box`, `vm/src/jit/helpers.rs`). |
| `../perf/perf-long-counted-loops-lose-the-optimizing-osr-body-20260918.md` | ArithProbe unchanged (single-pass body); the optimizing `mul` body is ~580 ms vs single-pass ~490 ms | Run the page's `mul` A/B with `CRATONVM_JIT_IR_CARRY_RCX_TWIN` on and off. Soak `CRATONVM_JIT_IR_PARTIAL_UNROLL` (needs `CRATONVM_JIT_IR_PER_COPY_FRAMES`) before any default flip. Back-edge move coalescing is still open. |
| `../perf/perf-zgc-tlab-chunk-zeroing-is-the-last-allocation-cost-20260918.md` | pristine skip helps only before the first GC; lazy zeroing (`CRATONVM_ZGC_LAZY_TLAB_ZERO`, OFF) engages but is neutral | Zero per object at allocation (JIT inline allocation plus collector), or zero chunks off the allocation path. Also identify the `vmovntdq` copy loop, about 5 % of `bintrees -Xmx512m`. |
| `aarch64-backend-has-ordering-encoders-but-no-shared-memory-lowerings-20260918.md` | `getstatic` (primitive) and `idiv`/`irem`/`ldiv`/`lrem` lower, the latter only in methods with no handlers | Add a null-pointer throw path with a per-site action code, then `getfield`/`putfield`/`arraylength`, bounds checks and `putstatic`. Nothing aarch64 can be compiled on the Windows x64 host (the `openssl-sys` cross build fails), so this needs an aarch64 build machine or CI. |

Not on a page of its own (recorded as an aside in the implicit-exceptions page, from lane
`review8c`, wave 8): **`TryLoop plain` is 7x slower than HotSpot.** The probe runs
`s += a[i] ^ i` with a `long` accumulator (`C:\craton\jitr9-probes\review8c\cls`):
203 ms against 29 ms, 1.5 against 0.22 ns per iteration. No SIMD reduction shape in the
vectorizer's detector (`jit/src/x64/simd_analysis.rs`) matches `a[i] ^ i`. This is a
detector-coverage gap, not a defect: extend the sum detector to a mixed `int`-element /
induction-variable XOR widened into a `long` accumulator.

## 3. Decisions for the owner

1. **`CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` stays default OFF.**
   - With it ON (HotSpot's default), a caught implicit exception costs 1.5-1.7 us instead of
     3.0-3.3 us.
   - It breaks the rule in `vm/tests/jit_npe_message_hot_equals_cold.rs`, and regression
     vector `RExceptions` fails with it on (verified on w10).
   - Turning it on means rewriting that vector's contract first.
2. **`CRATONVM_ZGC_LAZY_TLAB_ZERO`** measured neutral. Delete it, or keep it as the base for
   per-object zeroing.
3. **`CRATONVM_ZGC_PRISTINE_CHUNKS`** is default ON. Standalone `bintrees` gains about 8 %
   (never collects); CratonBench is neutral. Keep it, or retire it once per-object zeroing lands.

## 4. Housekeeping found at the merge

1. Two `native-builtins` ratchets fail on the merged tree. Round 9 registers no stubs and
   retires no triples: its native-builtins edits are `lang_math.rs`, `unsafe_natives.rs` and
   `deprecated_internal.rs` only. Both fail identically on plain `origin/dev` (4716a6ed1,
   checked 2026-09-19), so they are dev's own: re-baseline or fix them in the native-builtins
   lane that added the stubs (`2a0a0aeec` and its neighbours).
   - `stub_ratchet::synthetic_stub_count_does_not_regress`: 4545 against baseline 4479.
   - `unconstructed_carrier_gate::no_new_class_is_both_minted_and_retired`:
     `java/nio/HeapCharBuffer`.
2. `cargo fmt --check` flags 14 files that no round-9 wave touched in its last two waves, for
   example `native-api/src/lib.rs`, `types/src/lib.rs` and three `jit/tests/r9w5_*` files.
3. **The `RetainedCode` last-two-drop race is tolerated, not closed.**
   - At the merge, dev's `RetainedCode::drop` replaced round 9's `retained_holders` counter.
     The counter was removed. The reason: the round-9 `Arc::into_inner` re-wrap freed the
     allocation the code-range registry still points to.
   - Two concurrent last-two drops can therefore both drop plainly. `ExecutableBuffer::drop`
     rescues the mapping into the retirement queue, so this is not a use-after-free, but
     `published_code_free_audit().1` counts it. `jit/tests/r9w2_runtime2_cache_reader_guard.rs`
     now allows fewer than 50 per 500 supersedes.
   - An exact last-holder decision that keeps the original allocation alive, with no re-wrap,
     would make the count zero again.
4. **Missing native at startup on dev.** Every run of the merged binary logs
   `WARN ... Missing native method method=java/lang/ref/Finalizer.isFinalizationEnabled()Z mode=real-JDK mode`.
   It comes from dev, not round 9. The probe harness now strips ANSI-coloured log lines, so
   the probes still compare equal.
5. **A/B `CratonBenchC2 bind` against w11.** Both merged binaries measured about 1 920 ms
   (1 924 and 1 919), against 1 706 ms on `cratonvm-jitr9-w11.exe` earlier the same day. The
   row has ranged 1 597-1 832 ms over the waves, so this may be drift or a dev change. It was
   not run interleaved; do that before concluding anything.
6. **The flag inventory's default column cannot be trusted row by row.**
   `docs/config/flag-inventory.md` does not read the code: `tools/flag-census/render-inventory.py`
   derives "default-on" from whether the INVENTORY row has an `off_word`.
   - While refreshing the README on 2026-09-19, four rows were checked against their readers.
     Three were corrected:
     - `zgc-conc-start` (`Z_CONC_START_PERCENT_DEFAULT = 0`) and `zgc-generational`
       (default `false`) are default OFF but were listed default-on.
     - `default-heap-ergonomics` is default ON but was listed opt-in.
   - The fourth, `zgc-parmark`, is still listed default-on, and its code disagrees with itself.
     - `Z_PARMARK_DEFAULT_WORKERS = 0` (`gc/src/zgc.rs`), so marking is serial by default.
       The comment at `Z_CONC_START_PERCENT_DEFAULT` says parallel STW marking was reverted
       after costing +31 % to +153 %.
     - Yet `parallel_mark_workers`' comment still says "DEFAULT-ON since 2026-08-13", and
       `flag_groups::tests::the_zgc_kill_switches_expand_to_the_word_their_parsers_read_as_false`
       pins the row's `off_word: Some("0")` on that premise.
     - The ZGC owner should decide which is true and make the constant, both comments, the row
       and the test agree. The README and `docs/GC.md` describe the constant, so they say
       parallel marking is opt-in.
   - The other rows are unaudited. A test that runs each reader with the variable unset and
     compares against the row would close this.
7. Stale local branch: the main checkout `C:\craton\cratonvm` has `dev` checked out, 3 commits
   behind `origin/dev` before this merge. Fast-forward it when convenient (it has untracked
   files only).
