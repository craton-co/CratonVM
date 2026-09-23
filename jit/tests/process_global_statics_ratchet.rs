// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchet: the JIT may not grow new `static` declarations.
//!
//! A `static` in this crate is process state. Every VM in the process shares
//! it, including an embedded VM and a test that builds two. AGENTS.md forbids
//! process globals for compatibility state, and a 2026-09-12 JIT review found
//! the rule broken by the state it exists for:
//!
//! * `JIT_COMPATIBILITY_MODE` (`lib.rs`) latched with `fetch_max`, so a VM
//!   created in `--jdk-only` mode made every later VM in the process
//!   over-strict, forever;
//! * `DESPEC_SET` (`deopt.rs`) let one VM's despeculation verdicts strip
//!   speculations from another VM's compiles;
//! * `BACKGROUND_COMPILER` (`tiered.rs`) drained only the first VM's compile
//!   queue.
//!
//! All three are gone. The latch was deleted, because the policy is already a
//! per-compilation argument. The despeculation set is a `DespecRegistry` owned
//! by the VM's `JitRealm`. The compile workers are owned by each
//! `TieredCompilationManager`. See
//! `jit-compatibility-and-despec-state-per-vm-FIXED.md`.
//!
//! Most of the remaining statics are legitimate: helper addresses that are
//! process-invariant `fn` pointers, `OnceLock` caches of environment flags,
//! metrics counters, thread-locals. This test does not judge them. It only stops
//! the count from growing, so each new one has to be argued for in review
//! instead of arriving unnoticed.
//!
//! # What counts
//!
//! One per source line in `jit/src/**/*.rs` whose first non-blank text declares
//! a static item. That is, in order:
//!
//! 1. an optional visibility, `pub` or `pub(...)`, followed by whitespace;
//! 2. the keyword, followed by whitespace;
//! 3. an optional `mut` followed by whitespace;
//! 4. an ASCII identifier;
//! 5. optional whitespace, then `:`.
//!
//! That covers module-level statics, statics inside function bodies (the
//! `static CACHE: OnceLock<..>` inside a getter), `static mut`, and each
//! declaration line of a `thread_local!` block. Every file under `jit/src`
//! counts, `#[cfg(test)]` modules and `tests.rs` files included. Telling test
//! statics apart textually is fragile, and counting them all keeps the rule
//! exact and the number stable.
//!
//! Not counted: comment lines, `'static` lifetimes and bounds (never at the
//! start of a line), and a `static $name:` inside a `macro_rules!` pattern,
//! which is not an identifier. A declaration split so that its name sits on a
//! later line than the keyword would also be missed; rustfmt never produces one.
//!
//! The baseline was computed on 2026-09-12, after the two statics above were
//! removed, with the equivalent
//!
//! ```text
//! grep -rhE '^\s*(pub(\([^)]*\))?\s+)?static\s+(mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:' \
//!     jit/src --include=*.rs | wc -l
//! ```
//!
//! run from the repository root, which printed 744.
//!
//! The keyword is assembled at runtime. This file is under `jit/tests/`,
//! outside the scanned tree, but a literal needle would be one copy-paste away
//! from counting itself.

use std::path::{Path, PathBuf};

/// `static` declaration lines in `jit/src` on 2026-09-12. Lower it when a
/// static is removed; never raise it to make room for a new one.
// 2026-09-12: 744 -> 747 on merging into the JIT review branch. The three are
// test fixtures merged alongside this ratchet, not process state: the
// `HITS` marker-helper counters and the `FLAG` byte in the instanceof and
// return-narrowing tests (`ir_lower.rs` and `x64/tests.rs` test modules).
// 2026-09-12: 747 -> 717. The per-compile request (`CompileRequest`) retired
// the self-call-identity and local-handler thread-locals, and the direct-call
// helper cells became the per-compile `DirectHelperTable`.
//
// 2026-09-16: 717 -> 720, RAISED, which this comment is obliged to justify
// because the rule above says never to. The three are process-INVARIANT
// instruments, which is the category the failure message itself carves out
// ("a `fn` address, a cache of an environment flag ... make the case in
// review"). None is per-VM state and none is compatibility state, so none is
// the thing `jit-compatibility-and-despec-state-per-vm-FIXED.md` is about:
//
//   * `implicit_null.rs` +4 (5 -> 9). The faulting-PC table stopped being two
//     parallel arrays plus a cursor and became an open-addressed hash table:
//     it gained a per-slot chain array, a per-page bucket-head array and an
//     occupancy counter. The table was ALWAYS process-global and has to be —
//     it is read from a signal handler, where a lock is a deadlock and an
//     allocation is undefined — so this is the same one global growing three
//     more arrays, not three new globals. The change that motivated it is in
//     that module's header: `recover` was a linear scan of 32,768 atomics
//     inside a fault handler.
//   * `ir_lower.rs` +1. `OSR_ENTRY_SKIPPED_NOT_SENTINEL_FREE`, the census for
//     optimizing bodies that declined OSR entry stubs because
//     `try_osr` would refuse them anyway. A counter whose ZERO is meaningful
//     (it would mean every optimizing body in the run was call-free), and the
//     workspace pins `release_max_level_info`, so a `debug!` could not carry
//     it on a release binary.
//   * `backend_parity.rs` +1. The new module's census, same argument.
//   * `platform.rs` +1 (net; 15 -> 16). The JIT code arena's shared handle and
//     its census. An arena is process-wide by definition — its whole purpose
//     is that one reservation serves every compile in the process — so it
//     cannot be per-VM without giving up the address-space saving it exists
//     for. It is also inert by default (`CRATONVM_JIT_CODE_ARENA`, off) and
//     inert by construction on macOS/AArch64.
//
// 2026-09-16 (second sitting): 721 -> 724, three more instruments, same rule
// and the same obligation to name each:
//
//   * `implicit_null.rs` +1. `CHAIN_LOCK`, the striped mutex that lets a dead
//     retirement-chain node be UNLINKED rather than only tombstoned. The
//     chains were append-only, so a repeatedly-recompiled page grew a chain
//     every later retirement of that page walked in full — bounded only by
//     `CAP`, i.e. the linear sweep the chains replaced. Process-global for the
//     same reason the table is: it guards that table.
//   * `ir_evidence.rs` +1. `TRANSFORM_CENSUS`, per-`Transform` counts across
//     the process. `CompileRecord` only ever answered "did THIS compile do
//     anything", so nothing could say how often a transform FIRES — which is
//     why the 2026-08-27 scalar-deopt soak needed a purpose-built instrumented
//     run to discover that scalar replacement fired zero times on netty and
//     hibernate. A flag whose soak cannot tell "ran and was neutral" from
//     "never ran" cannot be flipped or retired.
//   * `lib.rs` +1. `BACKEND_PARITY_CENSUS_LOCK`, serialising the shadow
//     single-pass compile the backend-parity driver runs. Default-OFF
//     diagnostic (`CRATONVM_JIT_BACKEND_PARITY`).
//
// All three are process-INVARIANT instruments, not per-VM state and not
// compatibility state, which is the category the failure message carves out.
//
// 2026-09-16 (third sitting): 724 -> 726. `lib.rs` +2,
// `LOCK_COARSENING_PLANS_OFFERED` / `LOCK_COARSENINGS_APPLIED`. Escape
// analysis has produced lock-coarsening plans on every run since the pass
// landed and nothing ever applied one; the applier is now wired
// (`CRATONVM_JIT_LOCK_COARSEN`, default OFF). Both numbers, because either
// alone is unreadable: a zero `offered` is a statement about the workload, a
// non-zero `offered` with a zero `applied` is a statement about the
// interaction with lock elision and is the reading that sends someone to look
// at the order the two run in.
//
// 2026-09-16 (fourth sitting): 726 -> 727. `lib.rs` +1,
// `BACKEND_PARITY_CENSUS_LOCK`, serialising the shadow single-pass compile the
// backend-parity driver runs so two threads cannot interleave one comparison's
// census bumps. Default-OFF diagnostic (`CRATONVM_JIT_BACKEND_PARITY`).
//
// 2026-09-16 (fifth sitting): 727 -> 728. `ir_evidence.rs` +1, the `Once`
// guarding `exit_summary`, the shutdown printer for `TRANSFORM_CENSUS`.
// The third sitting added that census and called it "a reader" -- and it was
// not: nothing in the tree called `transform_census()`, so on a release
// binary the counts were accumulated and then discarded, which is precisely
// the shape (`release_max_level_info` compiles `debug!` away, so a counter
// whose only reader is a log line reads as zero) the census was added to
// escape. `exit_summary` is the reader, on both VM exit arms, under
// `CRATONVM_JIT_TRANSFORM_CENSUS`. A `Once` is process-invariant by
// construction: it exists so a one-shot print stays one-shot.
//
// 2026-09-16 (sixth sitting): 728 -> 730. `lib.rs` +2,
// `SCALAR_REPLACEMENT_CANDIDATES` / `SCALAR_REPLACEMENT_PLANS`. Both, for the
// same reason the lock-coarsening pair is both: the `scalar-replacement` row
// of the transform census counts allocations ELIDED, and a zero there cannot
// say whether escape analysis offered nothing or the planner refused
// everything it offered -- which is exactly the question that has kept
// CRATONVM_SCALAR_DEOPT parked. With the pair, the answer on a probe written
// to be scalar-replaceable is `candidates=0`: the analysis, not the planner.
// Process-invariant counts, read by `ir_evidence::exit_summary`.
//
// 2026-09-16 (seventh sitting): 730 -> 731. `escape_analysis.rs` +1,
// `SCALAR_REFUSAL_CENSUS`: thirteen counters, one per `ScalarRefusal` reason.
// The sixth sitting's pair established THAT escape analysis offers no
// scalar-replacement candidates; it could not say WHY, and this module's own
// doc comments name two very different culprits -- `Op::Call` arg-escaping
// every reference operand (so every `new` escapes to its own `<init>`) and
// `ScalarRefusal::LoadNotAnswerable`, documented in place as "the refusal a
// hot loop hits". A zero candidate count is compatible with both and
// actionable on neither. Counted at the single exit of
// `find_scalar_replacements` rather than at its fourteen `push` sites, so a
// later refusal arm cannot forget to register itself. Read by
// `ir_evidence::exit_summary`.
//
// Note `tiered.rs`'s `c2_hotness_bar` is NOT in this count: it is a field on
// `CompilerCore`, per-manager state rather than a process global, which is the
// distinction this ratchet exists to police.
//
// 2026-09-16 (eighth sitting): 731 -> 732. `escape_analysis.rs` +1,
// `SCALAR_SCOPE_CENSUS`, a two-element `[analyses run, allocation nodes seen]`.
// The seventh sitting's refusal census came back with all THIRTEEN rows at
// zero, which refuted both explanations the module's own comments offer and
// left three possibilities its rows cannot separate: the analysis never ran,
// it ran on a graph with no allocation node, or it ran on the wrong graph.
// These two numbers separate them, and did: `analyses=2,
// allocation-nodes-seen=0`, i.e. escape analysis runs and never sees the
// method that allocates. Read by `ir_evidence::exit_summary`.
//
// 2026-09-17 (ninth sitting): 732 -> 733. `entry_counter.rs` +1, the
// `OnceLock<bool>` that caches `CRATONVM_JIT_ENTRY_COUNTER`. A cached
// environment-flag read is the exact case the failure message carves out as
// legitimately process-invariant: the flag is read once per process and the
// answer cannot change, and it MUST be cached because the site that asks is
// `emit_prologue` -- a per-compile path where a `getenv` would be paid on
// every compiled method.
//
// 2026-09-17 (tenth sitting): 733 -> 739, from JIT review round 6. Six, in
// three groups, and none of them is per-VM state:
//
//   * `aarch64_backend.rs` +1, `INVALID_REGISTER_SEEN`. A `thread_local!`
//     `Cell<bool>`, not a process global at all — it is the sticky "an
//     `Arm64Register` was not a valid encoding" flag that replaced two
//     `.expect()` panics in the `r` / `fp` converters, read and cleared by
//     `emit_machine_code` for the compile that set it. Per-compile state on
//     the compiling thread; a second VM compiling concurrently has its own.
//     The alternative was threading a `Result` through several hundred
//     infallible call sites, which is what the panics were avoiding.
//
//   * `deopt.rs` +2, `STASH_NESTED` and `STASH_DROPPED_UNCLAIMED`. The census
//     for the deopt stashes after they became bounded nesting stacks. A
//     callee's stashed frame can still be discarded beneath the innermost one
//     when the caller deopts on the propagated sentinel before a sink claims
//     it; that residual is now MEASURED rather than silent, which is the
//     precondition for fixing it. Both are expected to read zero.
//
//   * `metrics.rs` +3, `ON`, `EPOCH` and `PRINT_SEQ`, all belonging to the
//     `-XX:+PrintCompilation` analogue. `ON` is a cached environment-flag
//     read, the case this test's own failure message carves out. `EPOCH` is
//     the process start instant the lines are stamped relative to, and
//     `PRINT_SEQ` orders them: both describe the PROCESS's output stream, and
//     a per-VM copy of either would make two VMs' lines unorderable against
//     each other, which is the opposite of what the flag is for.
//
// 2026-09-17 (eleventh sitting): 739 -> 743, the second half of review round
// 6. Four, and the same test applies: none is per-VM state.
//
//   * `platform.rs` +2, `SYNCED_CODE_EPOCH` and `ISB_ISSUED`. The first is a
//     `thread_local!` `Cell<u64>` holding the last code-publication epoch THIS
//     thread issued an `ISB` for, which is what turns the AArch64 reader-side
//     barrier from "before every compiled call" into one per thread per
//     publication; per-thread by construction. The second counts the barriers
//     actually issued, and is the only way to tell "never needed" from "never
//     wired" — the distinction the missing call site made for months. Both
//     compile out on x86-64.
//
//   * `ir_verify.rs` +1, `ENABLED`, a cached environment-flag read. Same
//     carve-out as `entry_counter.rs`'s in the ninth sitting, and for the same
//     reason: the asking site now runs once per BUILD as well as per optimize
//     pass, so an uncached `getenv` would be paid per compiled method.
//
//   * `x64/op_local_stack.rs` +1, `CAT2_MODEL_DISAGREEMENTS`. The three
//     category-2 width models were reduced to one consult point, with
//     disagreement treated as a refusal rather than a vote; this counts the
//     refusals. It exists to be zero, and a nonzero reading is the evidence
//     that the two surviving models have drifted — which is the thing the
//     reduction was for.
//
// 2026-09-17 (twelfth sitting): 743 -> 745, the last of review round 6. Two,
// and unlike the previous three sittings ONE OF THEM IS A KNOWN-IMPERFECT
// PLACEMENT rather than a clean process invariant. Recorded that way on
// purpose.
//
//   * `deopt.rs` +1, `LAST_TAKE_DISCARDED`, a `thread_local!` `Cell<usize>`
//     recording how many nested frames the most recent `take_last_deopt`
//     discarded beneath the innermost one. Per-thread by construction, and
//     read by the sink on the same thread that took the stash. Expected zero.
//
//   * `compile_gate.rs` +1, `RUNTIME_DESPECULATED`. This one is per-VM state
//     in a process global, and the lane that added it says so: it lives here
//     because `lib.rs` was out of its scope, and it is flagged for relocation
//     onto the VM's own JIT state.
//
//     What it buys is worth the interim: a runtime `MakeNotCompilable` used to
//     be filed under `mark_jit_bail_listed`, whose contract is "the backend
//     cannot emit this" and which `admit` asks at EVERY door — so one cold
//     `assert x : "msg " + y;` under `-ea` denied every unrelated hot loop in
//     that method its OSR entry for the life of the process. The registry is
//     what lets a runtime de-speculation verdict be a different thing from a
//     backend capability verdict.
//
//     The cross-VM exposure, stated rather than hoped away: the key is the
//     full `(ClassId, class, method, descriptor)` tuple, and two VMs in one
//     process routinely load the same class name, method and descriptor. So
//     one VM's de-speculation can suppress another's OSR for the same method.
//     That is a THROUGHPUT leak and not a correctness one — `admit` answers a
//     refusal, and a refused compile runs interpreted — which is why it is
//     admitted here instead of blocking the fix. It should still move; see
//     `NOTES-deopt2.md`.
//
// 2026-09-17 (thirteenth sitting): 745 -> 746, review round 7. One:
// `ir_schedule.rs`'s `IR_PRIORITY_CENSUS`, a nine-element counter array for
// the block-priority sort, built on the `PairCensus` idiom the crate already
// uses.
//
// Two of its nine buckets are MODEL INVARIANTS rather than refusals: `cycle`
// and `memory_order` count outcomes the scheduler's own argument says cannot
// happen — a cycle in a DAG it constructs, and a reordering the total
// side-effect chain should have forbidden. A non-zero reading there does not
// mean the scheduler declined something; it means the DAG construction
// argument or the chain's totality is wrong. A counter is the only way to
// tell those apart from "this shape never occurs", which is the same reason
// the eighth sitting admitted `SCALAR_SCOPE_CENSUS`.
//
// Process-invariant by construction: it counts the compiler's own decisions,
// not a VM's state, and two VMs compiling concurrently are both describing
// the same scheduler.
//
// 2026-09-17 (fourteenth sitting): 746 -> 747, review round 7's CHA lane. The
// arithmetic is minus one plus two, and the minus is the point.
//
// GONE: `compile_gate::RUNTIME_DESPECULATED`, the process-global map of runtime
// de-speculation verdicts. That is the static the TWELFTH sitting above admitted
// with its exposure spelled out -- "one VM's de-speculation can suppress
// another's OSR for the same method ... It should still move; see
// NOTES-deopt2.md". It moved. The verdicts now live on `JitRealm::runtime_despec`,
// per VM, and every READ names the VM it is asking about. This raise is that
// admission being paid off, not a new one being taken.
//
// A correction while we are here, because the twelfth sitting's reasoning was
// wrong even though its conclusion was right: it said the key being the full
// `(ClassId, class, method, descriptor)` tuple is what fails to separate two
// VMs. `ClassId`s are allocated per `ClassStore` from 0, so two VMs in one
// process collide on the ID as routinely as on the name -- the tuple never
// separated VMs at all. What it separates is two LOADERS inside one VM, which
// is what it was designed for.
//
// ADDED, both in service of removing the above:
//
//   `LIVE_REGISTRIES` -- a weak index of the per-VM registries, so the two
//   remaining process-wide entry points can reach them. Not compatibility
//   state: it holds `Weak` handles to state that now lives per VM, which is
//   the opposite of what AGENTS.md forbids.
//
//   `AMBIGUOUS_RUNTIME_MARKS` -- the counter that measures what the index
//   cannot route precisely.
//
// The one caller that could WRITE a verdict into the wrong VM through that
// index -- `mark_runtime_not_compilable`, called from
// `vm/src/jit/helpers.rs::DeoptimizationController::deoptimize` -- is gone as
// of NOTES-cha.md C1: that site holds a `&SharedVm` and now marks
// `vm.jit.runtime_despec` directly. The function is kept, with no production
// caller, because `AMBIGUOUS_RUNTIME_MARKS` staying at zero is the evidence
// that the residual is closed.
//
// The two consumers that still fan out are `forget_runtime_not_compilable_for_class`
// (class unload, from `tiered.rs::invalidate_class`) and
// `runtime_not_compilable_size` (a diagnostic). Forgetting too much costs at
// most one compile attempt; counting too much costs nothing. Neither can write
// a verdict against a VM that did not earn it, which was the whole exposure.
//
// TO GET TO ZERO: thread an `Arc<RuntimeDespecRegistry>` into `TieredCompiler`
// at construction, the way the VM already hands it to everything else. Then
// `invalidate_class` forgets on its own VM's registry, the diagnostic can take
// one as an argument, and both statics can be deleted -- LOWERING this number
// by two rather than raising it. That is a plumbing change through the tiered
// manager's constructors and did not belong in the same commit as the fix.
//
// 2026-09-17 (fifteenth sitting): 747 -> 749. TWO cached environment-flag
// reads, from two round-8 lanes that raised this constant independently and
// each labelled itself the next sitting. Reconciled into one entry on
// integration, because two raises of +1 landing as a single +2 is exactly the
// arithmetic a reader of this file needs to be able to follow.
//
//   `lib.rs`   +1  `OnceLock<bool>` in `code_cache_sweep_enabled`, caching
//                  `CRATONVM_JIT_CODE_CACHE_SWEEP` (RT-8's cold-body sweeper).
//   `ir.rs`    +1  `OnceLock<bool>` in `guard_token_edges_enabled`, caching
//                  `CRATONVM_JIT_IR_GUARD_TOKEN` (`Op::Guard`'s token edge).
//
// Both take the same carve-out as the ninth and eleventh sittings: a cached
// read of a process environment variable, which cannot differ between two VMs
// in one process, on a path where an uncached `getenv` would sit under the
// compiler. `code_cache_sweep_enabled` is asked once per sweep and once from
// the code-cache cap's one-time warning; `guard_token_edges_enabled` is asked
// by the `String` expansion and by every `idiv`.
//
// Worth stating explicitly, because a sweeper is exactly the kind of thing that
// usually arrives as process state: the sweeper's STATE is not here. The
// per-artifact count snapshots, the rate-limit reading and the sweep tally live
// on `JitCache::sweep`, a field, because the bodies and the usage signal are
// one VM's. Only the flag is a static.
//
// 2026-09-17 (sixteenth sitting): 749 -> 750. ONE cached environment-flag read.
//
//   `ir_check_elim.rs` +1  `OnceLock<bool>` in `range_edges_enabled`, caching
//                          `CRATONVM_JIT_IR_RANGE_EDGES` (B3, the path-
//                          sensitive value-range refinement).
//
// Same carve-out as the ninth, eleventh and fifteenth sittings: a cached read
// of a process environment variable, which cannot differ between two VMs in one
// process, on a path where an uncached `getenv` would sit under the compiler.
// It is asked once per `ir_check_elim::analyze`, i.e. once per optimizing
// compile.
//
// The ANALYSIS state is not here, and that is the part worth saying out loud:
// `PathRanges` -- the per-block narrowing maps, the census, the user index --
// is a value returned from `analyze_graph_pathwise` and dropped when the
// compile ends. It is derived from one graph and one schedule and means
// nothing outside them, so there is no version of it that could be shared
// between two VMs even by accident. Only the flag is a static, and it is a
// static for the same reason its two siblings
// (`CRATONVM_JIT_IR_BCE_RANGE`, `CRATONVM_JIT_IR_CHECK_ELIM`) are.
//
// 2026-09-17 (seventeenth sitting): 750 -> 751. ONE `thread_local!`.
//
//   `lib.rs` +1  `JIT_THREADS_HELD: Cell<u32>`, the count of `jit_threads()`
//                guards the CURRENT THREAD holds, read by
//                `lock_retire_queue` to report taking the retirement queue
//                under `jit_threads()` (NOTES-runtime.md RT-3 item 4).
//
// This is not process state in the sense this ratchet exists to police. A
// thread-local cannot be observed by another thread, let alone another VM,
// and this one is a count of guards whose lifetime it exactly brackets: it is
// zero whenever the thread holds no `jit_threads()` guard, which is every
// moment outside two short critical sections. There is no value it can carry
// from one VM's compile into another's. It is a `thread_local!` precisely
// BECAUSE the rule it checks is per-thread -- "this thread must not take the
// queue while holding the other lock" -- and a process-wide count would answer
// a different, wrong question (whether ANY thread holds it).
//
// It is counted here because the scanner counts declarations, and a thread-local
// is one. The alternative -- teaching the scanner to exempt `thread_local!` --
// would exempt every future one without reading it, which is the opposite of
// what a ratchet is for.
//
// 2026-09-18 (eighteenth sitting): 751 -> 753. `ir_evidence.rs` +2, the
// admission funnel.
//
//   `ADMISSION_EXITS` -- six counters, one per `AdmissionExit`: where each
//   optimizing request left the IR tier. `ir_evidence::census` counted only
//   the acceptance gate, the last of four independent exits that all produce
//   the same observable (a single-pass body), so the size of the population
//   the tier never sees was unknown -- which is how an IR-tier regression test
//   passed on three methods that never reached the tier
//   (`ir-tier-admission-hides-its-own-defects-FIXED-20260918.md`). Process-
//   invariant for the reason `TRANSFORM_CENSUS` is: it counts the compiler's
//   own decisions, not a VM's state. Read by `ir_evidence::exit_summary` and
//   the `[c2-supersede]` census.
//
//   `IR_EXIT_MARK` -- a `thread_local!` `Cell` naming the stage that declined
//   the IR attempt running on this thread, read back by the driver when
//   `ir_tier` returns and saved/restored around each attempt so a nested
//   compile cannot leave its verdict for the outer one. Per-compile state on
//   the compiling thread, like the tenth sitting's `INVALID_REGISTER_SEEN`.
//
// If a later change removes any of these, LOWER this number; the rule is
// unchanged and the next raise owes the same paragraph.
//
// 2026-09-18 (nineteenth sitting): 753 -> 753. One out, one in, both
// `deopt.rs`, from retiring the nested-deopt-chain (M3) record.
//
// GONE: `LAST_TAKE_DISCARDED`, the twelfth sitting's thread-local. It existed
// so a sink could refuse to resume a frame at an invoke when the take had
// discarded a "nested callee" beneath it. There is no such callee: the
// ordinary stash only ever receives re-execute points, and neither backend
// stashes a caller's frame when a callee returns the sentinel, so a frame
// under the top is a stale leftover, never a live callee. The refusal was
// declining correct resumes, and the witness went with it. This also corrects
// the tenth sitting's line above ("when the caller deopts on the propagated
// sentinel"): callers do not.
//
// ADDED: `STASH_NON_REEXECUTE_REFUSED`, the counter for
// `ordinary_stash_frame`, which now ENFORCES the invariant the argument above
// rests on -- a point that is not a re-execute point is turned into the
// re-run sentinel instead of entering the stash. Process-invariant: it counts
// a producer defect, which is the same in every VM. It exists to be zero.
// 2026-09-18 (JIT review round 9): 751 -> 754. Three declarations, none of
// them state that can differ between two VMs.
//
//   `x64/simd_analysis.rs` +1  `OnceLock<bool>` caching
//                  `CRATONVM_JIT_VECTORIZE` for the sum detector -- the same
//                  cached-environment-flag carve-out as the sittings above.
//   `implicit_null.rs`     +1  `STALE_DUPLICATES_RETIRED`, a should-read-zero
//                  diagnostic counter for the implicit-null site table. The
//                  table it counts is already one of this file's eleven
//                  statics (the fault handler has no VM to ask); the counter
//                  is a tally of that table's own repairs, not new state.
//   `ir_lower.rs`          +1  a `static FLAG: u8` inside a `#[cfg(test)]`
//                  function: the poll word an executed test hands the
//                  compiled loop. Test-only, never linked into the VM.
//
// (`layout_const_inventory.rs`'s two gate-pass counters changed type, from a
// shared `AtomicU64` to a `StripedCounter`, and are not a change in count.)
//
// 2026-09-18 (JIT review round 9, wave 2): 754 -> 769. Fifteen declarations,
// in four kinds, none of them state one VM could observe of another's:
//
//   Should-read-zero / event census counters (6) -- the same kind every
//   exit-summary census in this crate already is:
//     `ir_lower.rs`      `IR_BUFFER_OVERFLOW_RETRIES`
//     `ir_schedule.rs`   `IR_INCOMPARABLE_INPUT_PLACEMENTS`
//     `lib.rs`           `CODE_BUFFER_PROTECT_FAILURES`,
//                        `LATE_LAMBDA_ADAPTERS_WITHDRAWN`
//     `profile_store.rs` `REPLAY_PENDING_CLASSES`
//     `ir_evidence.rs`   `INPUTS_GENERATION` -- the generation stamp of the
//                        IR-refusal memo, which is itself already a static in
//                        that file; a stamp on a process table is not a second
//                        table.
//   The per-thread engagement counters' backing (3), `lib.rs`
//     `PER_THREAD_EVENT_BLOCK` (a `thread_local!`), `PER_THREAD_EVENT_BLOCKS`
//     and `PER_THREAD_EVENT_EXITED`: the eight `*_SERVED` / `*_DECLINED`
//     statics stopped being shared `AtomicU64`s (one locked add per helper
//     call) and became per-thread stripes; these three are the stripes'
//     registry and the exited threads' fold. Same count of counters, no new
//     meaning.
//   Per-compile caches (1): `ir_optimize.rs` `SNAPSHOT_LIVENESS_ON`, a
//     `thread_local!` holding one compile's reading of
//     `CRATONVM_JIT_IR_SNAPSHOT_LIVENESS` for the passes of that compile.
//   Test-only hooks (5): `ir_lower.rs` `IR_FIRST_ATTEMPT_CAPACITY_FOR_TEST`,
//     `R9W2_AASTORES`, `R9W2_TYPE_CHECKS` and a poll-word `FLAG` inside
//     `#[cfg(test)]` code, and `platform.rs` `FAIL_NEXT_MAKE_EXECUTABLE`, the
//     `make_executable` fault injection the finalize-abort tests drive.
//
// 2026-09-18 (JIT review round 9, wave 3): 769 -> 775.
//
//   Per-compile thread-locals (2): `ir.rs` `SPLICE_RESUME_THIS_BUILD` and
//     `ir_optimize.rs` `SPLICE_RESUME` carry one build's splice-resume map from
//     the builder to the optimizer of the SAME compile; set and taken inside it.
//   Flag cache (1): `lib.rs`, a `OnceLock<bool>` -- the carve-out above.
//   Census counter (1): `lib.rs` `OBJECT_HASHCODE_STRING_GUARD_SITES`.
//   Test-only (1): a poll-word `FLAG` in an `ir_lower.rs` `#[cfg(test)]` test.
//   Policy set (1): `lib.rs` `JIT_STICKY_DISPATCH_METHODS`, the methods whose
//     cycle-closing call must stay on `jit_invoke_dispatch`. It NARROWS the
//     existing `JIT_RECURSIVE_CYCLE_METHODS` (every cycle member, forever) to
//     the closing edges, and is process state for the same reason that one is:
//     it is keyed by method name, and a stale entry only costs another VM a
//     dispatched call, never an answer. Moving both onto `JitCache` is the
//     per-VM follow-up (docs/known-issues/jit/perf-mutual-recursion-*).
//
// 2026-09-18 (JIT review round 9, wave 4): 775 -> 780. Five test-only
// declarations: `x64/objects.rs`'s executed test of the ZGC-announce post-init
// mode keeps its stub helpers' call counts and arguments in `NOTE_CALLS`,
// `NOTE_ARGS`, `POST_INIT_CALLS`, `POST_INIT_ARGS` and `NEW_OBJECT_CALLS`
// (an `extern "C"` stub has nowhere else to put them). `#[cfg(test)]`, never
// linked into the VM.
//
// 2026-09-18 (JIT review round 9, wave 6): 780 -> 788.
//   Census counters (6): `ir_optimize.rs` `IR_DCE_NODES_KILLED`,
//     `IR_WRITE_ONLY_STORES_REMOVED`, `IR_LIVENESS_GRAPHS`,
//     `IR_LIVENESS_UNATTRIBUTABLE`, `IR_LIVENESS_SNAPSHOTS` and
//     `IR_LIVENESS_UNCONSULTABLE` -- the snapshot-liveness census the
//     `ir-snapshots-at-every-bci` page asked for, printed by the VM's
//     `[c2-supersede]` census beside the existing IR counters. Monotonic
//     diagnostics; no answer depends on them.
//   Test-only (2): two more poll-word `FLAG`s in `ir_lower.rs` `#[cfg(test)]`
//     executed tests (the lcmp-fusion and edge-resolution loops).
//   (deopt6's per-compile baked-table record was moved onto `Compiler`
//   instead of a thread-local.)
//
// 2026-09-19 (merge of JIT review round 9 with the eighteenth/nineteenth
// sittings on dev): both lines started at 751. Round 9 added the 37
// declarations above (751 -> 788) and the sittings added 2 (751 -> 753), so
// the merged count is 790.
//
// 2026-09-20: 790 -> 792. Two TEST FIXTURES, not process state, and neither
// arrived with a baseline raise -- the ratchet has been red on `dev` since the
// first of them merged, which is how they were found. The scanner counts every
// declaration line under `jit/src` including `#[cfg(test)]` modules, on
// purpose (see "What counts" above: telling test statics apart textually is
// fragile, and counting them all keeps the number exact), so a test fixture
// still owes this paragraph.
//
//   * `x64/tests.rs` +1, `TEST_THROW_BCI`, merged at `00cc38bea`. One more
//     line of the `thread_local!` block that already holds `TEST_NPE_ACTION`
//     and `TEST_NPE_TRAP_KEY`, both of which this baseline already counts: it
//     records the bytecode position the null-check deopt stub published, so
//     the null-NPE tests can assert the stamp. Read only by the test stand-in
//     helper in the same file, and `jit/src/x64.rs` gates the module
//     `#[cfg(test)] #[cfg(target_arch = "x86_64")]`, so it is never linked
//     into the VM.
//   * `ir_lower.rs` +1, an eighth poll-word `FLAG`, from `e93e1f1e2`
//     (`r9w12_the_long_counted_loop_shapes_all_admit_an_optimizing_osr_body`).
//     The same `static FLAG: u8 = 0;` the 2026-09-12 and 2026-09-18 entries
//     above already justified twice: a function-local byte inside an executed
//     `#[cfg(test)]` test, taken by ADDRESS as the safepoint poll word. Its
//     value is never read and it holds nothing between tests.
//
// Neither is per-VM state and neither is compatibility state, so neither is
// what `jit-compatibility-and-despec-state-per-vm-FIXED.md` is about.
//
// 2026-09-21 (JIT review round 9, waves 12-13): 792 -> 796. Four TEST-ONLY
// declarations, all in `aarch64_backend.rs`'s `arm64_execution` module, which
// is gated `#[cfg(target_arch = "aarch64")]` INSIDE a `#[cfg(test)]` module --
// so it is not linked into any VM, and on x86-64 it does not exist at all.
// They are the same shape, and for the same reason, as the 2026-09-18 wave-4
// entry above: an `extern "C"` stub has nowhere but a static to put what it
// was called with.
//
//   * `NPE_CALLS` and `NPE_ACTION` -- the fake `jit_npe_with_action`'s call
//     count and the packed action word it was handed, so the executed
//     `arraylength` test can prove a null receiver reaches the NPE stub AND
//     reports that opcode's own JEP-358 action.
//   * `AIOOBE_CALLS` and `AIOOBE_ARGS` -- the same for the fake
//     `jit_throw_aioobe`, whose four arguments (index, length, array, bci) are
//     the whole contract the per-site bounds stub marshals.
//
// Neither is per-VM state and neither is compatibility state, so neither is
// what `jit-compatibility-and-despec-state-per-vm-FIXED.md` is about.
//
// 2026-09-21 (JIT review round 9, wave 15): 796 -> 798. Two more TEST-ONLY
// declarations in the same `#[cfg(target_arch = "aarch64")]`-inside-`#[cfg(test)]`
// module, of the same shape and for the same reason as the four above.
//
//   * `PUTFIELD_CALLS` and `PUTFIELD_ARGS` -- the fake `jit_putfield_*`'s call
//     count and the three arguments it was handed (receiver, field index,
//     value). They are the whole contract of the first helper call this
//     backend makes from the MIDDLE of a method, and the only way to show that
//     a null receiver does NOT reach it is to count the calls that do.
//
// Neither is per-VM state and neither is compatibility state, so neither is
// what `jit-compatibility-and-despec-state-per-vm-FIXED.md` is about.
//
// 2026-09-21 (JIT review round 9, wave 16): 798 -> 803. Five more TEST-ONLY
// declarations in the same `#[cfg(target_arch = "aarch64")]`-inside-`#[cfg(test)]`
// module, of the same shape and for the same reason as the six above.
//
//   * `GETFIELD_CALLS`, `GETFIELD_ARGS` and `GETFIELD_RESULT` -- the fake
//     `jit_getfield`'s call count, the three arguments it was handed
//     (vm_ptr, obj_ptr, field_index) and the value the test staged for it to
//     return. The vm_ptr is the whole point: it is what proves the context ABI
//     put the caller's context in X0 and shifted every Java argument up one.
//   * `THREW_CALLS` and `THREW_ANSWER` -- the same for the fake
//     `dispatch_threw`, whose answer decides whether a `long` field's
//     `i64::MIN` is `Long.MIN_VALUE` or a pending exception. A test that could
//     not stage both answers could not show the difference.
//
// Neither is per-VM state and neither is compatibility state, so neither is
// what `jit-compatibility-and-despec-state-per-vm-FIXED.md` is about.
//
// 2026-09-21 (JIT review round 9, wave 17): 803 -> 806. Three more TEST-ONLY
// declarations in the same `#[cfg(target_arch = "aarch64")]`-inside-`#[cfg(test)]`
// module, of the same shape and for the same reason as the eleven above.
//
//   * `PUTSTATIC_CALLS`, `PUTSTATIC_ARGS` and `PUTSTATIC_RESULT` -- the fake
//     `jit_putstatic_*`'s call count, the four arguments it was handed
//     (vm_ptr, class_id, field_index, value) and the value the test staged for
//     it to return. The staged return is what lets one test show BOTH halves
//     of the sentinel contract: `0` falls through to the body, `i64::MIN`
//     leaves the frame.
//
// Neither is per-VM state and neither is compatibility state, so neither is
// what `jit-compatibility-and-despec-state-per-vm-FIXED.md` is about.
//
// 2026-09-21 (JIT review round 9, wave 18): 812 -> 815. Three more TEST-ONLY
// declarations in the same `#[cfg(target_arch = "aarch64")]`-inside-`#[cfg(test)]`
// module, of the same shape and for the same reason as the fourteen above.
//
//   * `ALLOC_CALLS`, `ALLOC_ARGS` and `ALLOC_RESULT` -- the fake
//     `jit_new_object`/`jit_newarray`/`jit_anewarray_object`'s call count, the
//     three arguments it was handed, and the fake object pointer the test
//     staged for it to return. One fake serves all three helpers because they
//     share a shape; the staged return is what lets one test show that a fresh
//     reference comes back as the result and a `0` leaves the frame with the
//     deopt sentinel instead of being pushed as null.
//
// Neither is per-VM state and neither is compatibility state, so neither is
// what `jit-compatibility-and-despec-state-per-vm-FIXED.md` is about.
//
// 2026-09-21 (JIT review round 9, wave 19): 815 -> 818. Three more TEST-ONLY
// declarations in the same `#[cfg(target_arch = "aarch64")]`-inside-`#[cfg(test)]`
// module, of the same shape and for the same reason as the seventeen above --
// and DELIBERATELY three rather than the fifteen five separate fakes would
// have taken.
//
//   * `REF_CALLS` -- one counter per reference helper (`putfield_object`,
//     `putstatic_object`, `aaload`, `aastore_type_check`, `aastore`), because
//     the thing `aastore` has to prove is that the store does NOT run once the
//     type check refused it, and that is a count.
//   * `REF_ARGS` -- eight words, not four: `aastore` calls two helpers at one
//     bytecode, and a test that could see only the last call's arguments could
//     not show that the array and the value survived the first.
//   * `REF_RESULT` -- what `aaload` hands back and what the type check
//     answers, staged by the test so one body can be run down both paths.
//
// Neither is per-VM state and neither is compatibility state, so neither is
// what `jit-compatibility-and-despec-state-per-vm-FIXED.md` is about.
//
// 2026-09-22 (JIT review round 9, waves 21-23): 818 -> 830. Twelve more
// TEST-ONLY declarations in the same `#[cfg(target_arch = "aarch64")]`-inside-
// `#[cfg(test)]` module, of the same shape and for the same reason as the
// twenty above. Every one of them exists because these waves' claims are about
// what happens INSIDE a call, and the only way to see that from a compiled
// body is to make the helper itself the witness.
//
//   * `FRAME_BASE`, `SP_ID_OFF`, `SEEN_SP_ID` (wave 21) -- the frame base the
//     prologue published, the offset the test staged, and the safepoint id the
//     fake allocation helper read back out of the RUNNING frame. That one
//     read is the whole wave: it performs the runtime's own two steps from
//     where a collector would stand, and it is the test that would have caught
//     the frame-record gap the day wave 18 shipped it.
//   * `DISP_CALLS`, `DISP_VM`, `DISP_INFO`, `DISP_N`, `DISP_ARGS`,
//     `DISP_RESULT` (wave 22) -- the fake `jit_invoke_dispatch`'s call count,
//     its four arguments, and the value the test stages for it to return.
//     `DISP_ARGS` is four words because the helper's argument BUFFER is the
//     thing under test: it is read out of the caller's live frame, and the
//     claim is that `args[0]` is the deepest operand.
//   * `TC_CALLS`, `TC_ARGS`, `TC_RESULT` (wave 23) -- the same three for
//     `jit_checkcast` / `jit_instanceof_check`, which share one fake because
//     they share a shape. The staged result is what lets one body be run down
//     the hit, the miss and the ClassCastException paths.
//
// None is per-VM state and none is compatibility state, so none is what
// `jit-compatibility-and-despec-state-per-vm-FIXED.md` is about.
//
// 2026-09-22 (scalar replacement under precise exception frames): 830 -> 831.
// One `OnceLock<bool>` inside `lib.rs::scalar_under_precise_frames_enabled`,
// the kill switch for admitting scalar replacement in a method compiled with
// precise exception frames. It is the "cache of an environment flag" case this
// rule names as acceptable, and it is byte-for-byte the shape of its neighbour
// `precise_handler_frames_enabled` -- one latched read of
// `CRATONVM_JIT_SCALAR_UNDER_PRECISE_FRAMES`, no per-VM and no compatibility
// state, so `jit-compatibility-and-despec-state-per-vm-FIXED.md` does not
// apply. The switch exists because the change behind it widens what the
// compiler emits in the area this tree's longest comment history is about, and
// a binary that can answer "is it this?" in one run is worth one static.
//
// 2026-09-22 (the collection thin-direct-helper OSR census): 831 -> 833.
// `HASHMAP_GET_DIRECT_SITES_OSR` and `HASHMAP_PUT_DIRECT_SITES_OSR` in
// `lib.rs`, two `AtomicU64`s incremented by the VM crate's
// `jit_bridge::compile_osr_artifact`. They are the OSR door's copies of the two
// `HASHMAP_*_DIRECT_SITES` counters five lines above them and are exactly the
// same shape: a process-lifetime count of bind sites, read by a report, written
// relaxed on a cold compile path. Neither is per-VM state and neither is
// compatibility state, so `jit-compatibility-and-despec-state-per-vm-FIXED.md`
// does not apply, and the reason they are a SEPARATE pair rather than more adds
// into the existing ones is the whole point of the change that added them: a
// prediction checked against counters that cannot see the OSR door is confirmed
// by construction.
//
// ATTRIBUTION, because this raise is not the work of the commit that makes it.
// The two statics arrived in `473df2db1` ("fix(jit): both direct compile doors
// bind through the policy, and the helper items are out of their reach"), which
// did not move this number, so `dev` carried a red `jit_static_declarations_do_
// not_grow` from that commit until here. MEASURED: over the 53 commits touching
// `jit/src` between `56616250e` (the 830 -> 831 raise) and this one, exactly one
// adds a `static` declaration the scanner counts, and it is that one; the tree
// at that commit's parent counts 831. Raised rather than referred back because
// the account is complete and unambiguous -- one commit, two statics, both of a
// kind this file already admits by name -- which is the condition the paragraph
// rule is asking for, not the identity of whoever types it.
//
// If a later change removes any of these, LOWER this number; the rule is
// unchanged and the next raise owes the same paragraph.
const BASELINE: usize = 833;

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => panic!("cannot read {}: {e}", dir.display()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Whether `line` declares a static item, by the rule in the module doc.
///
/// `keyword` is the item keyword, passed in so the literal never appears here.
fn declares_static(line: &str, keyword: &str) -> bool {
    let mut rest = line.trim_start();

    // 1. Optional `pub` / `pub(...)`, which must be followed by whitespace.
    if let Some(after_pub) = rest.strip_prefix("pub") {
        let after_vis = match after_pub.strip_prefix('(') {
            Some(inner) => match inner.find(')') {
                Some(close) => &inner[close + 1..],
                None => return false,
            },
            None => after_pub,
        };
        let trimmed = after_vis.trim_start();
        if trimmed.len() == after_vis.len() {
            return false;
        }
        rest = trimmed;
    }

    // 2. The keyword, followed by whitespace (so `statics` or `static_x` is not it).
    let Some(after_keyword) = rest.strip_prefix(keyword) else {
        return false;
    };
    let trimmed = after_keyword.trim_start();
    if trimmed.len() == after_keyword.len() {
        return false;
    }
    rest = trimmed;

    // 3. Optional `mut` followed by whitespace. `mutex` is a name, not `mut`.
    if let Some(after_mut) = rest.strip_prefix("mut") {
        let trimmed = after_mut.trim_start();
        if trimmed.len() != after_mut.len() {
            rest = trimmed;
        }
    }

    // 4. An ASCII identifier.
    let ident_end = rest
        .char_indices()
        .take_while(|&(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    if ident_end == 0 {
        return false;
    }

    // 5. Optional whitespace, then the type annotation's colon.
    rest[ident_end..].trim_start().starts_with(':')
}

#[test]
fn jit_static_declarations_do_not_grow() {
    let keyword = ["sta", "tic"].concat();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "found no Rust sources under {}; the scan would pass vacuously",
        root.display()
    );

    let mut count = 0usize;
    let mut per_file = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let n = text
            .lines()
            .filter(|line| declares_static(line, &keyword))
            .count();
        if n > 0 {
            let shown = path
                .strip_prefix(&root)
                .unwrap_or(path.as_path())
                .display()
                .to_string();
            per_file.push((shown, n));
        }
        count += n;
    }
    assert!(
        count > 0,
        "counted no static declarations under {}; the scanner is broken, not the crate clean",
        root.display()
    );

    if count > BASELINE {
        let listing = per_file
            .iter()
            .map(|(file, n)| format!("  {n:>4}  {file}"))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "jit/src declares {count} statics; the baseline is {BASELINE}.\n\
             \n\
             A static in cratonvm-jit is shared by every VM in the process. \
             New per-VM or compatibility state belongs on the VM's JIT state \
             (`JitRealm` on the VM side, or `JitCache`), threaded into the \
             compile request as an argument, not in a static. AGENTS.md forbids \
             process globals for compatibility state. See \
             jit-compatibility-and-despec-state-per-vm-FIXED.md.\n\
             \n\
             If the new static is genuinely process-invariant (a `fn` address, a \
             cache of an environment flag), remove another one or make the case \
             in review. When a static is removed, lower BASELINE in \
             jit/tests/process_global_statics_ratchet.rs to the new count.\n\
             \n\
             Per file:\n{listing}"
        );
    }
    if count < BASELINE {
        eprintln!(
            "note: jit/src now declares {count} statics, below the baseline of \
             {BASELINE}. Lower BASELINE in \
             jit/tests/process_global_statics_ratchet.rs to {count} so the \
             removal cannot be undone silently."
        );
    }
}

/// The scanner itself, against lines whose answer is known. A scanner nobody
/// has watched fail passes vacuously.
#[test]
fn the_scanner_counts_declarations_and_nothing_else() {
    let kw = ["sta", "tic"].concat();
    let counted = [
        format!("{kw} FOO: u8 = 0;"),
        format!("    {kw} CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();"),
        format!("pub {kw} BAR: AtomicU64 = AtomicU64::new(0);"),
        format!("pub(crate) {kw} BAZ: &str = \"x\";"),
        format!("pub(super) {kw} mut QUX : i32 = 1;"),
        format!("    pub {kw} mut __jit_debug_descriptor: JitDescriptor = JitDescriptor {{"),
        format!("        {kw} DEPTH: Cell<usize> = const {{ Cell::new(0) }};"),
        format!("{kw} mutex: Mutex<()> = Mutex::new(());"),
        format!("{kw} STR: &'{kw} str = \"\";"),
    ];
    for line in &counted {
        assert!(declares_static(line, &kw), "must count: {line:?}");
    }
    let ignored = [
        format!("// {kw} FOO: u8 = 0;"),
        format!("    /// {kw} FOO: u8 = 0;"),
        format!("fn f() -> &'{kw} str {{ \"\" }}"),
        format!("fn g<T: '{kw}>(t: T) {{}}"),
        format!("    {kw} $name: $ty = $init;"),
        format!("{kw}s: u8"),
        format!("let x = {kw}_value;"),
        format!("publish {kw} FOO: u8 = 0;"),
        format!("pub{kw} FOO: u8 = 0;"),
        String::new(),
    ];
    for line in &ignored {
        assert!(!declares_static(line, &kw), "must not count: {line:?}");
    }
}
