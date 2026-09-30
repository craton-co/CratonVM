# Proposal: per-method back-edge counters and OSR verdicts (HotSpot `MethodCounters` shape)

**Status: open — filed 2026-09-23 by interpreter round i1, lane L7.** Proposal;
stage 1 not landed in wave 2 (see "Progress (wave 2)"); wave 16 made the
steady-state back edge cheaper without the struct and found the struct's
premised home is per call site, not per method (see "Progress (wave 16)");
wave 20 took the OSR budget consult off the string hash and the lock, and
the struct still waits on the fixture step (see "Progress (wave 20)").

## Progress (wave 20)

Interpreter round i1 wave 20, lane L5. Re-derived from the current code
again; the landed stage is the OSR-budget half of the wave-16 "next stage",
without the struct.

**What the consult cost.** `osr_loop_offer_allowed` (`jit_bridge.rs`) is
asked once per activation and loop header, at the first offer past the
1 000-back-edge floor. As long as no loop in the VM had a whole activation
refused it cost one load. After the first such record -- one un-OSR-able
loop anywhere -- every first offer of every header of every method built
`osr_optimizing_refusal_key` (a SipHash over the VM identity, the method
name and the descriptor) and took `osr_refused_loops`' mutex to find
nothing; `note_osr_loop_entered` did the same after every OSR entry.

**Landed** (`vm/src/runtime/interpreter/jit_bridge.rs`):

* `osr_loop_key(frame, entry_pc)` is `(class id, fingerprint, pc)`, the
  fingerprint being the low half of the method's invocation-counter key
  (`cratonvm_jit_api::invoc_key_parts`). A cached frame reads it from its
  entry's memo (`CachedBytecodeMethod::invoc_key`), so no string is hashed;
  an owned frame computes the same value, so both kinds of frame share one
  record. The VM identity left the key: the map is the VM's own. A collision
  of the 64-bit fingerprint could only share a budget (whether OSR is
  offered), never what runs; the optimizing-OSR memos, which hand out code,
  keep `osr_optimizing_refusal_key`.
* `JitBridgeState::osr_refused_loop_filter`, a 256-bit summary of the map's
  keys (`osr_loop_filter_bit`), rewritten with the count under the map's
  lock after every change (`publish_osr_refused_loops`: note, forget, the
  epoch discard, the purge). `osr_loop_offer_allowed` and
  `note_osr_loop_entered` skip the lock when the header's bit is clear
  (`osr_loop_may_have_record`). A clear bit is exact as of the last publish;
  the only race is the one the count already had (a record published while
  a consult is in flight is seen by the next consult).
* `vm/src/runtime/interpreter.rs` (`pop_and_recycle_frame_with_reason`): the
  default-off `CRATONVM_JIT_LOOP_WORK_TIERUP` harvest takes a cached frame's
  memoised `invoc_key` instead of hashing the names at every pop (same key).
* Tests (`jit_bridge.rs`, `i1w20_l5_tests`, at the end of the file):
  `a_cached_and_an_owned_frame_of_one_method_share_the_loop_key` (and a
  budget spent through one is spent for the other),
  `the_filter_follows_the_refused_loop_map` (one record one bit, an entry
  and an epoch purge take the bit away, the filter empties with the map),
  `a_header_without_a_record_is_answered_without_the_lock` (answers while
  another thread holds the map's lock). `the_loop_budget_key_is_per_vm_method_and_pc`
  now asserts per-VM through the map rather than the key.
* Bench: `tools/probes/interp/L7/BackEdgeCounterBench.java` (header
  updated). The saving is one SipHash of two strings plus an uncontended
  lock per activation that reaches the floor, so the JIT-on "long" phase
  should be flat to slightly faster, and only in a run where
  `CRATONVM_DBG_OSR=1` shows an `all-offers-refused` line; `--nojit` never
  reaches the consult.

**Still not landed, and why.** The per-method `MethodCounters` record
itself. Its one frame-reachable home is still a `CachedBytecodeMethod`
field (per call site, carrying an `Arc` to the per-method record), and a
field is still ~70 struct literals: `jit-api/src/lib.rs` 7, `jit/src/lib.rs`
7, `jit/src/tests.rs` 18, twelve `jit/tests/*.rs` files 32, plus four in
`vm/`. The unblocking step is unchanged since wave 6 and belongs to a round
that owns the jit crate: an `impl Default for CachedBytecodeMethod` (or a
`test_fixture(..)` constructor) in `jit-api`, adopted by those fixtures with
`..Default::default()`. After it:

1. `MethodCounters { backedges: AtomicU32, osr_loops: [(pc, refused, epoch)]
   small inline table }` per VM, created at the first `from_parts` for the
   method through a per-VM map keyed by the same `(class id, fingerprint)`
   this stage introduced, so the record and today's budget map agree on
   identity; `osr_loop_offer_allowed` then reads the frame's entry and never
   the map.
2. Flush `backward_count` into `backedges` at pop (plus the back edges
   `record_osr_background_pending` discards), replacing the loop-work
   harvest's sharded lock.
3. The combined tier-up predicate and decay, each with the interleaved A/B
   the "Risk" section asks for.

## Progress (wave 16)

Interpreter round i1 wave 16, lane L5. Re-derived from the current code; the
stage that landed is the one that makes the steady-state back edge cheaper,
and the per-method struct itself did not land, for a reason found on the way.

**What the steady-state back edge cost.** Every back edge bumps
`Frame::backward_count` and compares it with the dispatch loop's
`osr_call_floor` (1 000); past the floor it calls `try_osr_with_backoff`.
Once a loop header's offers are spent for the activation (five refusals, the
method-wide `osr_loop_offer_allowed` budget, a denial, an un-enterable
published artifact, a refused background request), `should_try_osr` answers
"no" forever -- but the call was still made on EVERY back edge past the floor
for the rest of the activation: two cached flag loads, the virtual-thread
test, the threshold read and a scan of `osr_attempt_counts`, out of line.
Under `--nojit` it was worse: the OSR door did not check
`CRATONVM_DISABLE_JIT` at all, so each loop header's first offers queued a
background OSR compile (starting the compiler threads under `--nojit`) for
the worker to refuse with `JitDisabled`, and every later back edge made the
call above.

**Landed.**

* `vm/src/runtime/frame.rs`: `Frame::osr_poll_at`, the `backward_count` at
  which the frame's next out-of-line poll is due (0 = at the floor), reset with
  `backward_count` (`reset_cached_tail`, `record_osr_background_pending`, every
  constructor).
* `vm/src/runtime/interpreter.rs`: `try_osr_with_backoff` is now a wrapper
  around the old body (`try_osr_offer`); on every `Skip` it stores
  `osr_next_poll(frame, entry_pc, threshold)` -- the header's own next due
  point (`threshold << attempts`, the expression `should_try_osr` compares; never
  for a retired header) or `OSR_POLL_STRIDE` (256) back edges on, whichever is
  sooner (HotSpot's back-edge notify frequency). The three back-edge sites
  (`cond_branch_arm!`, `goto_arm!`, the decoded path) compare
  `bc >= osr_call_floor && bc >= frame.osr_poll_at`: the second compare runs
  only past the floor, so a loop below it pays nothing new. A due header keeps
  being polled every back edge; a single-loop frame is offered at exactly the
  same back edges as before; a second loop header of the same frame is offered
  at most 256 back edges late. The arrival trace and
  `CRATONVM_JIT_NO_OSR_INLINE_GATE=1` keep the call on every back edge
  (`osr_poll_point_armed`); `OSR_POLL_POINT_ENABLED` is the `const` kill switch.
  Near `u32::MAX` the poll point is 0 (every back edge until the count wraps),
  so a frame is never left with a poll point it cannot reach.
* `--nojit`: `osr_call_floor` is `u32::MAX` under `env_cache::disable_jit()`,
  and `try_osr_offer` declines on the same flag (after the arrival trace), as
  the method-entry door already did. No OSR can be entered under `--nojit`, so
  nothing a program computes changes; the background compile threads are no
  longer started by the OSR door, and the `osr_method_denied` events it used to
  record under `--nojit` are gone.
* Tests (`vm/src/runtime/interpreter/jit_bridge.rs`, `i1w16_l5_tests`):
  `the_next_poll_is_the_headers_due_point_or_a_stride_away`,
  `a_single_loop_is_offered_at_the_same_back_edges_with_fewer_calls` (offers at
  1000/2000/4000/8000/16000 with and without the poll point; 39 001 calls
  against fewer than 200 over 40 000 back edges),
  `a_declined_offer_leaves_the_frames_next_poll`.
* Bench: `tools/probes/interp/L7/BackEdgeCounterBench.java` -- sixteen loop
  shapes, a "long" phase (5 000 iterations per call, past the floor every
  activation) and a "short" control (200 per call). The long phase should drop
  under `--nojit` and with the JIT on for any loop whose OSR is refused; the
  short phase should be flat.

**Why the struct did not land: `CachedBytecodeMethod` is per call site.** The
proposal puts `backedges` / `osr_attempts` / `osr_denied_epoch` "on
`CachedBytecodeMethod`, next to `interp_invocations`" as HotSpot's per-METHOD
`MethodCounters`. In the current code a `CachedBytecodeMethod` is built per
invoke-cache fill -- `dispatch_static::populate_invoke_cache` and
`dispatch_virtual::populate_virtual_invoke_cache` each `from_parts` a fresh one
per (call site, thread), and the shared promotion table is keyed by call site
-- and only the padded code and the
quickened stream are shared per method (`padded_bytecode_for_method`, a
process-wide memo). `interp_invocations` is per call site for exactly this
reason and is batched into the per-method `ProfileStore` counter. A back-edge
counter there would split one method's loop work over its call sites and
threads, and an OSR verdict there would be one cache in front of the per-VM
`JitBridgeState` record per site. The fixture obstacle recorded in waves 2 and
6 still stands besides (≈40 struct literals, most in the jit crate's tests).

**Next stage, re-derived.**

1. Give the method ONE per-VM counters record: an `Arc<MethodCounters>`
   (`backedges`, `invocations`, the OSR verdict stamped with
   `JitRealm::code_state_epoch`) created where the method is linked and
   reachable from the `Class` method table, and carried into every
   `CachedBytecodeMethod` built for it (one `Arc` field, set by `from_parts`
   from `CachedMethodParts`; the jit-crate fixtures take a `Default`). Frames
   without a cached method keep the per-frame counters.
2. Flush `backward_count` into `backedges` at pop (replacing the default-off
   `CRATONVM_JIT_LOOP_WORK_TIERUP` harvest's hash + sharded lock). Note that
   `record_osr_background_pending` restarts `backward_count` while a
   background OSR compile is pending, so the flush must add the back edges it
   discards there too, or the count is a lower bound.
3. The combined tier-up predicate and decay, as proposed; each needs the
   interleaved A/B the proposal's "Risk" asks for.

## Progress (wave 8)

Stage 2's OSR half landed without the struct field: the method-wide OSR
budget lives in the VM's `JitBridgeState` (`osr_refused_loops`, one record per
loop header of a method, per install epoch), asked once per activation and
header rather than per back edge — see
`docs/internal/fixed-bugs/interpreter-L7-osr-refusals-reoffered-per-activation-FIXED-20260925.md`, "Progress (wave 8)".
It is a hashed lookup, not the frame-reachable counter this page proposes, so
`osr_attempt_counts` stays (it now also marks "the budget was asked"); the
rest of the proposal (a decayed per-method back-edge counter, the combined
tier-up predicate) is unchanged and still waits on the
`CachedBytecodeMethod` fixture change described below.

## Progress (wave 6)

Still not landed; the obstacle has moved but not gone. Every PRODUCTION
`CachedBytecodeMethod` is now built through `CachedBytecodeMethod::from_parts`
(wave 5), which adds the memo cells empty — so a new counter field is a
one-line change there. The struct literals that remain are the test fixtures,
and they are in the jit crate: `jit/src/lib.rs` (7), `jit/src/tests.rs` (18),
eight `jit/tests/*.rs` files (23 between them), plus `jit-api/src/lib.rs` (10)
and one each in `jit_bridge.rs` and `wave1_adoption_tests.rs`. Adding a field
breaks all of them, and the jit crate belongs to another session this round.
The cheapest unblocking step is a jit-api `impl Default for CachedBytecodeMethod`
(or a `test_fixture(..)` constructor) adopted by those fixtures with
`..Default::default()`, done in a jit round; the counters can then land as
proposed.

Meanwhile wave 6 took more of stage 2's target without new state:
`compile_osr_artifact`'s standing refusals (the force-interpret levers,
registered-native, and `synchronized` under background compilation) now
`mark_osr_denied`, so they too cost one attempt per install epoch instead of
five per activation (`docs/internal/fixed-bugs/interpreter-L7-osr-refusals-reoffered-per-activation-FIXED-20260925.md`).

## Progress (wave 2)

Not landed, for a mechanical reason worth recording before someone tries:
`CachedBytecodeMethod` (`jit-api/src/lib.rs`) is built by struct literal at
**23 sites in 13 files** (`deopt_resume.rs` 5, `jit_bridge.rs` 3, `frame.rs` 2,
`interpreter.rs` 2, `vtable.rs` 2, `jit/helpers.rs` 2, `dispatch_static.rs`,
`dispatch_virtual.rs`, `lambda.rs`, `lockfree_resolve.rs`, three test files).
Adding the three counter fields touches all of them — six lanes' files at
once in a round where each lane owns a disjoint set. Do it as its own change,
and take the chance to add a `CachedBytecodeMethod::new(..)` (or a
`from_code_attr` builder) first so the NEXT field is a one-file change; the
two `jit_bridge.rs` callee literals and the `deopt_resume.rs` ones are
already near-identical copies.

What wave 2 did land on the same problem, without new per-method state:
bytecode-determined OSR refusals (unbridged `invokedynamic`, unsupported
`ldc` kinds) now `mark_osr_denied`, so they cost one attempt per install
epoch instead of five per activation — see
`docs/internal/fixed-bugs/interpreter-L7-osr-refusals-reoffered-per-activation-FIXED-20260925.md`. That removes the
worst case this proposal's stage 2 targets; stage 2 remains the general fix.

## Today

* The OSR trigger counts back edges PER FRAME (`Frame::backward_count`,
  `vm/src/runtime/frame.rs:350`), reset on every activation. A loop of a few
  hundred iterations per call never reaches the 1 000 threshold, however many
  calls there are. `interpreter.rs`'s own comment (`try_osr_with_backoff`)
  records the consequence: Tomcat's BCEL annotation scan reports `osr=0`.
* The workaround, `CRATONVM_JIT_LOOP_WORK_TIERUP` (default off), harvests
  `backward_count / 32` into the method invocation counter at frame pop, and
  then has to re-nominate by hand because the dispatch site only acts on exact
  threshold crossings (`cnt == threshold || (cnt - threshold) % 64 == 0`).
* The OSR retry budget (`osr_attempt_counts`, five attempts) is also per frame,
  so refusals are re-offered per activation
  (`docs/internal/fixed-bugs/interpreter-L7-osr-refusals-reoffered-per-activation-FIXED-20260925.md`).
* The invocation counter lives in a sharded `RwLock<HashMap<u128, AtomicU32>>`
  (`jit/src/profile.rs` `add_loop_work` / invocation shards), reached by hashing
  a packed key per count.

## Proposal

Put the counters where HotSpot does — one small struct per method, reached from
the frame without hashing:

```rust
// on CachedBytecodeMethod (jit-api), next to `interp_invocations`
pub backedges: AtomicU32,         // decayed, not reset per activation
pub osr_attempts: AtomicU8,       // method-wide OSR retry budget
pub osr_denied_epoch: AtomicU32,  // install epoch of the last deny
```

* Back edge: `frame.backward_count += 1` stays (it is register-cheap) and is
  FLUSHED into `backedges` at pop and at each OSR poll, so short loops
  accumulate across calls.
* Tier-up predicate: `invocations + backedges / K >= threshold` evaluated at the
  existing dispatch site (one extra relaxed load), replacing the loop-work
  harvest and its hand-rolled re-nomination.
* OSR poll: `backedges >= osr_threshold << osr_attempts` — one method-wide
  budget; `mark_osr_denied` becomes a store to `osr_denied_epoch`.
* Decay: halve `backedges` and `invocations` on the existing
  `maybe_drain_jit_entry_counters` sweep (HotSpot's counter decay), so a method
  hot once at startup does not stay "hot" forever.
* Frames without a `CachedBytecodeMethod` (launcher `main`, synthetic frames)
  keep today's per-frame behaviour.

## Staged plan

1. Add the fields, flush-at-pop only, behind `CRATONVM_JIT_METHOD_BACKEDGES=1`;
   print both counts under `CRATONVM_DBG_TIERUP_DECLINE`.
2. Switch the OSR budget to `osr_attempts`; delete `osr_attempt_counts`.
3. Replace `loop_work_tierup` with the combined predicate; measure Tomcat's
   annotation scan (`osr=` and compile counts) and CratonBench.
4. Add decay.

## Expected benefit

Short-loop-heavy methods (parsers, per-class metadata readers) tier up; OSR
refusals stop costing a compile attempt per activation; one relaxed load
replaces a hash + sharded lock per counted invocation on the loop-work path.

## Risk

Tier-up policy changes move benchmarks both ways; every stage is flag-gated and
needs an interleaved A/B (see the microbenchmark-noise note: medians of
interleaved runs).
