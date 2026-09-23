# JIT review round 9: what is left to do

**Status:** OPEN (index), and narrow. Written by the integrator at the close of round 9
(waves 1-11, 2026-09-18 .. 2026-09-19), when the branch `fix/jit-review-r9-20260918` was
merged into `dev`. Re-audited 2026-09-22 in three passes (lanes h23, h23b and
h23c).

**The whole §1 adversarial review is now closed**: all twelve sub-items reviewed, two of them
producing real fixes (the OSR re-entry memo's VM-identity ABA, and a missing throw-bci stamp
on the aarch64 `getfield` sentinel edge), and two producing the executed reproducers the
index asked for and wave 9/11 had not written (the retire-cell withdrawal under running
callers, the CHM wrapper walk under a real resize). The CHM item and six of seven
housekeeping items are closed. On the aarch64 successor page, the exception-table gap,
monitors, both reference-field gaps and `multianewarray` are closed -- that
backend now lowers every opcode it can be asked to, and the shared-memory gate
that policed the remainder is inert.

What is left: `cargo fmt` debt too large and too contested to sweep in one pass (§4.2), the
`RetainedCode` race (deliberately left as documented, tolerated debt, §4.3), and what the
aarch64 page still lists -- and that page's headline has changed. Its blocker is no longer a
lowering: **the OSR door is x86-64 only, so no loop that backend compiles is ever entered.**
`JitLocalHandlers`, `invokedynamic`, the performance items and real hardware all sit
behind that -- a body reached only through a door that is shut cannot be measured, which
is why the hardware ask comes last rather than first. (`multianewarray` left this list on
2026-09-22: the N-dimensional helper landed VM-wide and both backends lower it.)
Every item below is either an open page in this directory (the page holds the evidence, the
numbers and the next step) or a standalone item written out here in full because it has no
page of its own.

Measure A/B interleaved against a previous binary in the same session: this machine's speed
drifts between hours. The probes are in `C:\craton\jitr9-probes\` (harness: `run.sh <exe> <label>`;
per-wave A/B scripts: `ab10.sh`, `ab11.sh`).

## 1. Correctness (fix first)

1. **CLOSED.** `chm-segment-resize-holds-the-stripe-write-lock-across-an-allocation-20260918.md`
   is FIXED (`docs/internal/chm-segment-resize-holds-the-stripe-write-lock-across-an-allocation-FIXED-20260920.md`).
2. **The wave-10 adversarial review, h23 pass (2026-09-22).** Not the whole checklist --
   three items below are still open -- but nine of twelve got a real pass: code read closely,
   and where a Java reproducer could settle the question, one was written and run against
   HotSpot 21 (`regression-suite/probes/`).
   - **Private instance self-call** (`CRATONVM_JIT_INSTANCE_SELF_CALL`): REVIEWED, SOUND.
     `H23SelfCallReview.java` exercises a subclass receiver, a `synchronized` method, nestmates
     and deep recursion into a dedicated-stack `StackOverflowError` -- identical output,
     line for line, against HotSpot and against `cratonvm` x64 with
     `CRATONVM_JIT_THRESHOLD=20`. `self_call_stack_guard` (`vm/src/jit/helpers.rs`) is a real,
     purpose-built native-stack-floor check baked before every direct self-recursive call; a
     genuine null self-call receiver cannot be written in source (`this` is never null).
   - **Wave 11's OSR re-entry memo** (`CRATONVM_JIT_OSR_REENTRY_MEMO`): REVIEWED, **BUG FOUND
     AND FIXED**. It keyed its per-thread cache entry on `shared as *const SharedVm as usize`
     -- the VM's raw address, not `SharedVm::vm_identity` (a per-process monotonic counter
     that can never repeat). `vm_init.rs` already carries the fix for exactly this shape of bug
     in a neighbouring side table ("the two VMs traded heap objects and the reader segfaulted").
     A host that destroys a VM and starts another on the same thread running the same class --
     not a stretch for an embedded VM restarted with the same application -- could reuse the
     freed address and hand the new VM a `CompiledMethod` compiled against the old VM's heap
     to OSR-enter. Fixed by switching both directions of the memo to `vm_identity`.
   - **`cached_osr_body_is_despec_stale`**, `osr_guard_exit_reason`, `guard_reason_for_trapped_frame`:
     REVIEWED, SOUND. Existing dedicated tests (`r9w10_call10_osr_despec_stale_tests`,
     `r9w11_osr11_tests`) all still pass.
   - **The IR box/unbox fold** (`ea_ir_bridge.rs`): REVIEWED, SOUND. `ir_box_readers` refuses
     the fold (returns `None`, no fold at all) unless EVERY reader of the boxed value is
     exactly a matching `Unbox` reading the receiver slot -- so a box compared with `==`,
     stored, or read again after `intValue()` is never folded in the first place; identity and
     escape are never at risk. `Integer.valueOf(v).intValue() == v` is true by the JLS
     regardless of whether `valueOf` returned a cached or fresh instance, so the cache range
     does not matter to correctness either.
   - **The division-guard re-tier** (`vm/src/jit/helpers.rs`): REVIEWED, SOUND. Per-thread
     counting avoids a lock on an already-slow deopt path; the one loose end (the site key
     includes the raw `SharedVm` pointer, which a multi-VM ABA could alias) only mistimes WHEN
     a method re-tiers, a performance heuristic, never what it computes.
   - **The stackless door `initialisation_is_unobservable`** (`vm/src/runtime/exceptions.rs`):
     REVIEWED, SOUND. The `initialized` check runs before the `untouched` refusal specifically
     because the two are mutually exclusive class-init states, and the ordering is what lets an
     already-initialized ancestor answer `true` instead of falling into the `false` arm.
   - **collect11's lock-free per-VM `Integer` cache table** (`native-builtins/src/lang_math.rs`):
     REVIEWED, SOUND. Already keyed by `vm_identity`, not the VM's address -- the multi-VM ABA
     the OSR memo above had does not apply here. The per-VM table directory is a genuine,
     acknowledged, documented leak (never removed, only ever added to) in exchange for a
     lock-free hit; not a defect, a disclosed tradeoff.
   - **The `CRATONVM_JIT_RETIRE_CELL` eviction race** (h23b): REVIEWED, SOUND, and the
     reproducer this item said was missing is now written and executed
     (`jit/tests/r9w9_evict9_retire_cell.rs::a_withdrawal_under_running_callers_is_never_observed_half_done`).
     The flag is DEFAULT-ON and the OSR door's edit is applied, so the stakes are every
     default run, not an opt-in. The window is between the site's `mov r11,[cell]` and its
     `CALL r11`: a thread that loaded a non-zero word is committed to entering that body,
     with no blocking point in between, after `JitCache::remove` has cleared the cell.
     What makes it safe is that `JitCycleEdgeCell::clear` hands the owner to the retirement
     queue (`defer_jit_owner`) instead of dropping it, so the mapping outlives any thread
     that could be inside it -- and `RetainedCode::drop`'s `strong_count() > 1` check
     cannot take the plain-drop path under a live frame, because
     `call_compiled_entry_under_owner` holds the callee's `Arc` across the call. 48 rounds
     x 3 workers, each round required to straddle the withdrawal so a pass cannot be
     vacuous; green in debug and release.
   - **chm11's `ConcurrentHashMap` wrapper-key chain walk** (h23b): REVIEWED, SOUND, with
     the missing half of the coverage written
     (`native-collections`'s `r9w11_chm_wrapper_get_survives_a_concurrent_resize`).
     Wave 11's own concurrency test cuts a chain IN PLACE inside one array; it never
     replaces the array, which is what a resize does. `chm_wrapper_chain_walk` is safe
     against that by construction -- it reads the bucket reference ONCE
     (`cursor_field_volatile_reference`) and takes the capacity from THAT array
     (`cursor_array_shape`), never from the segment again, so `map_bucket_index` is always
     evaluated against the array it then indexes. The new test resizes 4 <-> 8 under a
     reader, relinking every node, with the allocation outside the stripe guard (holding it
     across one is the separate fixed bug).
   - **The SpliceCast re-probe in `direct_callee_lookup`** (h23b): REVIEWED, SOUND. The
     raced body it binds to is subject to exactly the same publication-time validation as
     the `cached_body` fast path above it -- `has_indy_trap`, then `pin_jit_entry`, then
     `prepare_for_publication`'s `owner.retired` and `artifact_id == want` checks, with
     `CRATONVM_JIT_STRICT_CALLEE_ROOTS` defaulting to ON so an unpinnable baked callee
     refuses publication outright. One nit worth recording rather than changing: the
     comment above it claims "the callee never arrives at a tier its caller was not
     compiled against", and that was already untrue of the `cached_body` probe a few lines
     up, which takes whatever the cache holds. It is a performance preference, not a
     correctness property -- publication validates identity and liveness, not tier.
   - **The ZGC pristine-chunk skip** (h23b): REVIEWED, SOUND, and already covered by tests
     (`gc/src/arena.rs`'s `pristine_window_reports_only_never_written_bytes` and
     `pristine_window_excludes_relocation_targets_and_the_high_end`), so nothing was added.
     The lazy-TLAB-zeroing half of this item was moot before it was read -- that flag was
     removed, see §3.2 below. What is left rests on four things, each checked: the backing
     store is zero at birth under a contract with its own test
     (`reservation.rs`'s `committed_memory_is_zero_and_writable`); the window is MONOTONE,
     so it can only ever name bytes that were never handed out, which is what makes it
     immune to writers that bypass the doors (`stamp_forwarding_words` is named as exactly
     such a writer); all three doors that make arena bytes writable call `note_writable`
     (`hand_out`, `commit_for_relocation`, `commit_parallel_evacuation_region`); and
     `take_last_hand_out_pristine` is one-shot AND pointer-checked, so a second carver
     interleaving between hand-out and claim gets `None` and zeroes in full. Every failure
     direction is "zero more than necessary", never "skip zeroing memory that has data in
     it".
   - **All twelve sub-items are now reviewed.** Two produced fixes: the OSR re-entry memo's
     VM-identity ABA (above) and, in the aarch64 lane, a missing throw-bci stamp on
     `emit_getfield`'s helper-sentinel edge (see the successor page's §6).

## 2. Performance (open pages)

| page | where it stands (w11, interleaved) | next step |
|---|---|---|
| `../perf/perf-building-a-throwable-costs-2-microseconds-20260920.md` (successor; the implicit-exceptions page is RETIRED 2026-09-20) | `new SomeException("msg")` -- ordinary Java, nothing VM-minted -- costs 2.2 us against HotSpot's 0.3. With construction taken out of the way (`CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=1`) a caught implicit exception is now 406-654 ns against HotSpot 429-1 491 on the same host, so construction is what is left | Profile `ThrowCost 200000` before touching anything -- the page names four suspects in `create_exception_object` that differ by an order of magnitude in what they could be worth. The compiled round trip that page inherited is CLOSED (`CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS`, default ON): `../../internal/fixed-bugs/perf-implicit-exceptions-in-compiled-code-cost-microseconds-each-FIXED-20260920.md`. |
| `../../internal/retired/perf-a-collection-native-pays-the-checked-heap-api-per-field-20260920-RETIRED-20260921.md` §1 -- **RETIRED 2026-09-21**, the third page of this family to go | `ChmProbe get` 2 622 ms for 3 M gets against HotSpot 7 ms | DONE for the membership probe: a validate-once read region (`NativeHeapAccess::read_cursor`) is **15 %** of the row, measured in one binary against its own kill switch, with `ChmProbe sget` flat at 0.3 % as the control. What is left is the LAYOUT lookup, not the probe -- a per-`(class_id, field_count)` cursor resolved once per region. No measurement of its own yet, so it is a design note on `ReadCursor`, not an open row here. |
| `aarch64-backend-runs-no-java-on-a-real-machine-20260922.md` (successor; the call-target page is **RETIRED 2026-09-22**, `../../internal/retired/aarch64-backend-cannot-resolve-a-call-target-20260921-RETIRED-20260922.md`, the fourth aarch64 page to retire in two days) | **Nothing is refused any more (h23 / h23b / h23c, 2026-09-22).** Every trapping lowering stamps its own throw-site bci (`Arm64Backend::emit_stamp_throw_bci`), so the exception-table restriction is gone; monitors lower through `jit_monitor_enter`/`jit_monitor_exit`; reference `getstatic` and `getfield` lower; and `multianewarray` lowers at ANY arity through `multianewarray_n` (the dimension buffer is the operand area itself, so it costs one `SUB`). That was the last opcode `opcode_touches_shared_memory` named without an ordered lowering, so the shared-memory gate is now INERT. A whole-VM aarch64 run of a `synchronized` + `try`/`catch` + reference-field workload gives HotSpot's exact answer with no `arm64-refused` line anywhere, and the qemu jit suite was 2841/2841 at h23b. | **The blocker is one DOOR, not a lowering.** `compile_osr_artifact` (`vm/src/runtime/interpreter/jit_bridge.rs:924`) is `x86-64 ONLY` and returns `None` on aarch64, so every OSR request fails at `stage=entry` and is retried forever -- no loop-shaped method ever runs compiled, and the retries make `CRATONVM_JIT=arm64` measurably SLOWER than no JIT. Method-entry compiles publish real code AND it is entered (`try_call_compiled_entry` has no architecture gate); the eager first-call door is a third `x86-64 ONLY` early return, but a later compile rather than a hole. That is §8 of the page and the thing to do FIRST: `JitLocalHandlers`, monitors' inline CAS, `invokedynamic` (a VM-wide gap with its own page), the inline fast paths and the hardware ask all sit behind it, because measuring a backend whose loops are never entered measures the interpreter. |

**The ZGC TLAB chunk-zeroing page is CLOSED (2026-09-20, lane `ztz`)** and retired to
`docs/internal/performance`. Its row is gone from the table above: the chunk `memset`
was measured by deleting it, and the whole of it is worth 2.1 % of
`CratonBench bintrees -Xmx512m`. Two things it leaves behind, neither on this page's
critical path:

* the profile that closed it put **4.78 %** of that run in one `Vec<usize>` -- the sweep's
  dead-address list, 12.0 M entries (96 MB) per cycle, copied three times. That was the
  unidentified `vmovntdq` loop, and it is fixed in the same change (exact reservation, no
  concatenation copy into a fresh buffer, an adoption into `dead_scratch`): the sweep pause
  per cycle drops 23-25 % over two ten-pair rounds (lower in 8 of 10 and in 10 of 10), and
  23 % over sixteen `IrAllocLoop` pairs. The one copy still there is the shard
  concatenation, which needs a `prune_dead` that takes chunks -- or a `prune_dead` that is
  handed the few addresses the monitor tables hold instead of every dead base.
* `zgc_sweep_header_zero` is default-on but scoped to a YOUNG cycle, so a whole-heap cycle
  still memsets every dead object's whole body -- redundant with the allocator's chunk
  `memset` at the other end. Its own doc says promoting it needs its own measurement; that
  belongs to the concurrent-and-generational plan, not here.

Not on a page of its own -- it was an aside on the implicit-exceptions page, which
retired on 2026-09-20, so THIS paragraph is now the only record of it (lane `review8c`,
wave 8): **`TryLoop plain` is 7x slower than HotSpot.** The probe runs
`s += a[i] ^ i` with a `long` accumulator (`C:\craton\jitr9-probes\review8c\cls`):
203 ms against 29 ms, 1.5 against 0.22 ns per iteration. No SIMD reduction shape in the
vectorizer's detector (`jit/src/x64/simd_analysis.rs`) matches `a[i] ^ i`. This is a
detector-coverage gap, not a defect: extend the sum detector to a mixed `int`-element /
induction-variable XOR widened into a `long` accumulator.

**Closed since this index was written.**
`perf-long-counted-loops-lose-the-optimizing-osr-body` was RESOLVED on 2026-09-20 (round 9
wave 12, lane `osrlong12`) and is now
`../../internal/fixed-bugs/perf-long-counted-loops-lose-the-optimizing-osr-body-FIXED-20260920.md`.
Its three next steps were all taken. The `mul` A/B was run (the default single-pass body wins
by 6 %, and `CRATONVM_JIT_IR_CARRY_RCX_TWIN` is on in both arms of every number in the page).
`CRATONVM_JIT_IR_PARTIAL_UNROLL` and `CRATONVM_JIT_IR_PER_COPY_FRAMES` were soaked
(`divergent=0` each) and stay default OFF anyway, because the wave found that the unrolled
optimizing body is refused by `ir_evidence::accept` and, forced through with
`CRATONVM_C2_ACCEPT=always`, is 2.1x SLOWER -- so wave 11's "partial unrolling already beats
single-pass" was an artefact of two misleading diagnostics, both since fixed. Back-edge move
coalescing remains closed WONTFIX
(`../../internal/fixed-bugs/linear-scan-no-phi-coalescing-costs-a-move-per-carried-value-FIXED-20260918.md`).
`ArithProbe` now measures 3 007 ms against HotSpot's 2 394, i.e. 1.26x where the page opened
at 2.02x.

## 3. Decisions for the owner

1. **`CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` stays default OFF.**
   - With it ON (HotSpot's default), a caught implicit exception costs 1.5-1.7 us instead of
     3.0-3.3 us.
   - It breaks the rule in `vm/tests/jit_npe_message_hot_equals_cold.rs`, and regression
     vector `RExceptions` fails with it on (verified on w10).
   - Turning it on means rewriting that vector's contract first.
   - The alternative is to make construction itself cheap, which would need no contract
     change at all: `../perf/perf-building-a-throwable-costs-2-microseconds-20260920.md`.
     Now that the compiled round trip is gone, this flag is the only remaining lever on
     the 2.2 us, which is what makes the alternative worth pricing.
2. **`CRATONVM_ZGC_LAZY_TLAB_ZERO`: DECIDED 2026-09-20 -- removed.** Measured neutral on
   both hosts, and the direction it was the base for (per-object zeroing) is closed by a
   ceiling measurement: deleting the chunk `memset` outright is worth 2.1 % of
   `CratonBench bintrees -Xmx512m` and 0.8 % of `IrAllocLoop`. Keeping it cost a SOFT
   `Tlab::end` -- the word compiled code compares every inline bump against -- for a flag
   nobody would turn on.
3. **`CRATONVM_ZGC_PRISTINE_CHUNKS`: DECIDED 2026-09-20 -- stays default ON.** It is free,
   it covers the whole start-up regime (standalone `bintrees` never collects and gains
   about 8 %), and the 2.1 % ceiling above is what is left after it. Nothing retires it.

## 4. Housekeeping found at the merge

Re-audited 2026-09-22 (lane h23). Six of seven items below are closed or were never really
open; one (`cargo fmt`) is ambient debt no single pass can close in this repository's current
state, explained where it stands.

1. **CLOSED.** The native-builtins ratchets are a moving target in this repository -- dozens of
   concurrent lanes touch `native-builtins/src` every day, so the exact numbers this item named
   (4545 / 4479) were already five re-freezes stale by 2026-09-22. Re-measured and re-frozen to
   the tree's current state (`management`: 4868 -> 4941, `synthetic-jdk`: 4841 -> 4914; the total
   registration count barely moved in either, the file's own signature for "existing fakes
   relabelled `SyntheticStub`", not a new one written) --
   `native-builtins/tests/stub_ratchet.rs`'s own entries carry the account. The
   `unconstructed_carrier_gate` `HeapCharBuffer` failure is gone; both configurations pass all
   14 `stub_ratchet` tests and all 3 `unconstructed_carrier_gate` tests. **Caveat:** by the time
   this update lands, the ratchet may be red again from unrelated concurrent work --
   `docs/jdk-only/H9-2-the-stub-ratchet-is-already-red-on-dev-20260922.md` is one such
   in-flight instance, correctly left unfrozen pending attribution. Whoever hits it next should
   do the same: attribute or re-freeze with an account, never raise the number silently.
2. **STILL OPEN, larger than this item described, and still growing while it is read.**
   `cargo fmt --check` flagged 241 spots earlier on 2026-09-22 and **271** by the end of the
   same day, across nine crates (`classloading/`, `gc/`, `jit/`, `native-api/`,
   `native-builtins/`, `native-collections/`, `native-io/`, `vm/`, `vm-cli/`) -- not the 14
   this item named. That 241 -> 271 inside one day is the same "many concurrent lanes" story
   as the ratchet above, and it is the argument: a one-off sweep does not close a debt that
   accrues faster than a lane can land. NOT swept in this pass:
   a blanket `cargo fmt -- --write` right now would touch files that dozens of other active
   branches are mid-edit on, and a formatting-only diff on top of theirs is exactly the kind of
   noisy, unreviewable conflict worth avoiding. This wants either a CI-enforced pre-commit hook
   (so the debt stops growing) or a dedicated, coordinated sweep when the tree is quiet, not
   another one-off pass.
3. **NOT changed, by a deliberate decision.** Read `jit/src/lib.rs`'s `RetainedCode::drop`,
   `CompiledMethod::drop` and `ExecutableBuffer::drop` closely enough to understand the actual
   mechanism: `CompiledMethod::drop` runs exactly once, at the TRUE last `Arc` release (Rust's
   own guarantee, not this code's), so `unregister_jit_code_range` is already race-free by
   construction. `RetainedCode::drop`'s `Arc::strong_count() > 1` check is a fast-path
   OPTIMIZATION only -- deciding whether to route through the tidy `defer_jit_owner` queue or
   fall through to a plain drop that `ExecutableBuffer::drop`'s own rescue arm catches -- and
   getting that heuristic wrong just moves an event from the clean counter to the rescue one.
   Confirmed still safe: `published_code_free_audit().1` is a bookkeeping distinction, not a
   use-after-free, exactly as this item already said. An "exact last-holder decision" here
   means redesigning a lock-free JIT code-cache retirement path with no way to stress-test it
   against a live collector in this session -- too high a blast radius for too small a payoff
   (a cleaner audit number) to attempt without dedicated time. Left as documented, tolerated
   debt.
   **Strengthened by the h23b retire-cell review (§1.2)**, which had to establish the same
   property from the other end and found the missing half of the argument: the heuristic
   cannot be wrong in the DANGEROUS direction, because a thread executing inside a body
   implies a second strong reference to it -- `call_compiled_entry_under_owner`
   (`vm/src/jit/helpers.rs`) holds the callee's `Arc` across the whole call, and says so:
   "the caller's owning reference is what keeps the body mapped for the whole call". So
   `strong_count() > 1` is necessarily TRUE under a live frame, and the plain-drop path the
   check guards is reachable only when no frame can be inside. `ExecutableBuffer::drop`'s
   rescue arm is the second line, not the first.
4. **CLOSED.** `java/lang/ref/Finalizer.isFinalizationEnabled()Z` is implemented
   (`native-builtins/src/lang_system.rs`): this VM has no switch to disable finalization, so it
   answers `true` unconditionally. The startup WARN is gone.
5. **STALE, unreproducible.** `cratonvm-jitr9-w11.exe`, the binary this row measured against,
   no longer exists -- dev has moved hundreds of commits since. The comparison this item asks
   for cannot be re-run as written. If `CratonBenchC2 bind` throughput still matters, it needs a
   fresh baseline binary and a fresh interleaved A/B, not a rerun against an artefact from two
   days and hundreds of commits ago.
6. **`zgc-parmark` row CLOSED; the general audit test NOT attempted.** `Z_PARMARK_DEFAULT_WORKERS`
   really was reverted to `0` the day after this item's premise was written (2026-08-14), and
   nothing since had followed through: the code comment, the `INVENTORY` row's `off_word` and
   the kill-switch test all still said default-on. Fixed all three (`gc/src/zgc.rs`,
   `types/src/flag_groups.rs`) and regenerated `docs/config/flag-inventory.md`; the row now
   reads `opt-in | off`, matching `docs/GC.md`, which was right the whole time. The suggested
   general fix -- "a test that runs each reader with the variable unset and compares against the
   row" -- was not attempted: there is no registry mapping a flag's `INVENTORY` row to its
   reader function to iterate over, for any of the other ~1460 declared flags, and building one
   is its own multi-day infrastructure project, not a housekeeping item.
7. **Was never actually open.** The main checkout (`C:\craton\cratonvm`) is on `dev`, up to date
   with `origin/dev`, with a clean working tree.
