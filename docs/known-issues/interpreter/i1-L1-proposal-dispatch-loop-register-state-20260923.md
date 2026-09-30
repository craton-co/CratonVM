# Proposal: close the per-bytecode gap by keeping dispatch state in registers

**Status: open, Stage 0 census landed — filed 2026-09-23 by interpreter round i1, lane L1; plan refined by wave 2, re-assessed by wave 3; Stage 0 instrument landed by wave 4; wave 5 found the JDWP gate live in workspace builds and trimmed the back-edge refresh; Stage 2's stack-depth proof and its memo bit landed (unused) by wave 7, lane L1 (see Progress).** Proposal.

## Where the interpreter stands (measured claims already in the tree)

All figures are CratonVM `--nojit` against HotSpot 25 `-Xint` on the same host,
taken from the comments in `execute_frame_from_index`
(`vm/src/runtime/interpreter.rs`) and
`docs/internal/performance/interpreted-invoke-cost-350ns-RETIRED-20260911.md`:

| operation | CratonVM | HotSpot | ratio | source |
|---|---:|---:|---:|---|
| straight-line bytecode | 11.0 ns (2026-09-02) → ~7.5 ns (2026-09-05) | 0.86–0.95 ns | ~8–13x | `probes/Dispatch.java`, `FieldBurn.java` |
| one backward branch | 33–37 ns → median 23.5 ns (poll gate) → 11–14 ns (OSR floor, did not separate) | 4.9 ns | ~2.5–3x | `tools/probes/BackEdge.java` |
| `tableswitch` iteration | 209 ns | 18.2 ns | 11.5x | 2026-09-02 table |
| decoded path vs fast path | 5078 ms vs 2312 ms | — | 2.2x slower | `FieldBurn.java`, `--noverify` A/B |
| per-bytecode STW poll | ≈7% of an interpreter-bound loop | — | — | `perf-advisory-assessment.md` §4 |

The straight-line figure is the floor everything else sits on, and it is almost
entirely **fixed work per iteration**, not the work of the opcode.

## What one iteration of the fast path does today

Read off the loop as of this round (each item is a load, store, compare or
branch the template interpreter does not execute):

1. `pending_java_exception.is_some() | pending_runtime_error.is_some()` —
   two discriminant loads, one branch.
2. `stack_dump_possible && …` — one register test.
3. `stw_flag.load(Acquire)` — one load, one branch (kept deliberately; see the
   in-code note — on aarch64 this becomes `ldar`).
4. `thread.frames.frame_ptr(frame_idx)` + null check — bounds compare and an
   `imul` by `size_of::<Frame>()`.
5. `pc` load, `last_instr_pc` store.
6. `single_step_active` test.
7. `code.len()` load, `code_len = len - 2`, `saved_pc < code_len` compare.
8. `code.as_ptr()` load, three byte loads (`opcode`, `b1`, `b2`).
9. `&mut thread.frames[frame_idx]` — **a second** bounds-checked index and
   `imul` for the same frame item 4 already located (kept borrow-checked on
   purpose, for the `let _ = frame;` discipline).
10. The `match` jump.
11. In the arm, every push/pop indexes **two** `Vec`s (`slots`, `kinds`), each
    bounds-checked, and writes a kind byte even for category-1 values.
12. `frame.pc = saved_pc + n` store, then back to 1.

HotSpot's template interpreter keeps `bcp`, the expression-stack pointer, the
locals pointer and (for int/long/float) the top-of-stack value in machine
registers, and dispatches with `movzbl (bcp), rbx; jmp *table(,rbx,8)` at the
end of each template.

## The staged plan

Each stage is independently shippable, measured with interleaved arms of one
binary (in-JVM timings on the development hosts swing up to ~3x between reps,
so interleave arms and compare medians), and carries a kill switch.

**Stage 0 — measure what has never been measured.** The in-code note
("The one thing never tested is the INDIRECT BRANCH itself … Do not build
replicated dispatch sites before that number exists") is right. On a Linux
host, `perf stat -e branches,branch-misses,instructions,cycles` over
`Dispatch.java`, `BackEdge.java`, `SwitchBurn.java` under `--nojit`, next to
HotSpot `-Xint`. Add a `CRATONVM_DBG=opcode-pairs` census (a
`[[u64; 256]; 256]` per thread, dumped at exit, zero cost unarmed via a hoisted
bool) to rank fusion candidates by dynamic frequency — see
`i1-L1-proposal-superinstruction-coverage-20260923.md`.

**Stage 1 — frame-switch caching (small, low risk).** `frame_idx` changes only
at invoke, return, unwind and OSR — all of which `continue` the loop. Cache
`code_ptr`, `code_len`, `max_locals` and the frame pointer in loop locals,
refreshed only when a "frame changed" flag is set by those sites (the slow path
already does exactly this for `quick_code_ptr`). This removes items 4, 7 and
part of 8, and lets item 9 become the one frame derivation. Expected: a few
percent on straight-line code; verify on `Dispatch.java` `nocall`.

**Stage 2 — raw stack pointer under the verifier contract.** Once
`safe_for_fast_path` gates the fast path per method
(`docs/internal/fixed-bugs/interpreter-L1-fast-path-ignores-safe-for-fast-path-FIXED-20260923.md`), the `max_stack`
proof holds for every method that reaches it, and `ValueStack`'s unchecked
methods can use `get_unchecked_mut` / raw pointers for `slots` and `kinds`,
removing two bounds checks per push and pop. This is only sound after the gate
exists — do not do it first.

**Stage 3 — stop writing `kinds` for category-1 pushes.** The `ValueStack::kinds`
doc already names the real fix: consume the verifier's per-pc type maps so a
slot's category is a static fact. Until then, a cheaper interim: the kind byte
is only *read* for category-2 disambiguation and GC root classification, so
arms that push a provably category-1 non-reference (`iconst`, `iadd`, `i2b`,
…) could skip the write if `kinds` were guaranteed zeroed on pop — measure
whether the extra store on pop costs more than it saves before committing.

**Stage 4 — `pc` and `sp` in locals.** Keep `pc` (and, after stage 2, the stack
pointer) in loop locals across iterations and write them back to the `Frame`
only at *escape points*: every invoke/return, every call that takes `&mut
thread` or `&JvmThread` (safepoint, JVMTI, allocation, field/array arms that can
throw), and the slow path. `last_instr_pc` is only read by unwinding and stack
walking, which are all behind those same escape points, so its per-bytecode
store (item 5) moves to them too. This is the change with the largest expected
payoff (it removes the pc store/load round trip from every arm and makes the
arms small enough to inline their stack traffic into registers) and the largest
blast radius: every consumer of `Frame::pc` / `last_instr_pc` (GC stack scans,
`capture_full_trace`, JVMTI `GetFrameLocation`, the stack-dump hook, OSR entry)
must be audited to run only at an escape point. A debug-build assertion at each
escape point (`frame.pc == local_pc`) turns a missed write-back into a test
failure rather than a wrong stack trace.

**Stage 5 — top-of-stack caching for `int`.** With stage 4 in place, cache the
top int in a register-resident local between int-producing and int-consuming
arms (the `iload; iload; iadd; istore` shape then touches the stack array zero
times). Needs a "TOS valid" state bit or a two-state dispatch; only worth it if
stage 0's pair census shows int arithmetic chains dominate.

**Stage 6 — threaded dispatch, only if stage 0 says so.** Rust has no stable
guaranteed tail call (`become` is unstable), so direct threading means either
replicating the `match` at the tail of the hottest arms or a handler-table
design that historically loses to a single `match` in Rust. Build it only if
the branch-miss rate from stage 0 is material (HotSpot-style per-template
dispatch buys most when the single indirect branch mispredicts often).

## Expected benefit

Honest range: stages 1–3 are percent-level each; stage 4 is the one that can
move the ~8x straight-line ratio materially (the fixed cost per bytecode is
mostly loads/stores of state that would stay in registers); stage 6 is
unknown until stage 0 runs.

## Risk

Stages 1–3: low, local to the loop and `ValueStack`. Stage 4: high — a missed
write-back is a wrong line number, a wrong handler range, or a GC scan of a
stale stack depth. Stage it behind a kill switch and the debug assertion above.

## Progress (wave 2)

No stage was landed as a measured change: every stage here is a per-bytecode
cost claim, and this wave could not run a binary. What changed underneath the
plan, and what that does to it:

* **A frame-switch detection point now exists (feeds Stage 1).** The preamble
  compares the executing bytecode allocation (`code_ptr`, already loaded)
  against a loop local and recomputes the per-method fast-path admission only
  when it changes (`frame_fast_path_admitted`). That compare fires on exactly
  the transitions Stage 1 names (invoke, return, unwind, OSR), without having
  to instrument each of them. Stage 1's `code_len` / `max_locals` caching can
  hang off the same branch — with one caveat the original text missed: a
  recursive call keeps the same `code_ptr` but changes the frame, so anything
  cached that is per-FRAME (the frame pointer, `max_locals` after
  `effective_max_locals` clamping) must still be keyed on `frame_idx`, and the
  frame pointer additionally on `FrameStack::reloc_epoch()`.
* **Stage 2's precondition is NOT yet met, and the text above said it would
  be.** The per-method gate closes the fast path only for methods whose maps
  carry a veto. Methods with NO maps — CDS-cached classes, per-class
  `skip_verification`, `ClassState::Verified` shortcuts, synthetic frames —
  still reach the unchecked arms, so `max_stack` is not a proven bound for
  every method that runs there. Raw-pointer stack access (Stage 2) would turn
  today's bounded `Vec` panic for such a method into memory unsafety. Before
  Stage 2: either make those classes publish maps, or close the gate for
  "no maps" too (and measure what that moves, starting with CDS).
* **Superinstructions reduce the dispatch count this plan prices.** The
  javac loop shape now runs its test in one iteration and its step in one
  (`i1-L1-proposal-superinstruction-coverage-20260923.md`, Progress), so
  Stage 0's pair census should be taken on a build that has them, or it will
  rank pairs that no longer dispatch separately.
* One fixed per-back-edge cost is gone: the async-exception slot load in
  `backedge_poll_needed!` (see `docs/internal/fixed-bugs/interpreter-L1-async-exception-channel-is-dead-FIXED-20260923.md`).

## Progress (wave 3)

Stage 1 was re-evaluated against the loop as it stands and deliberately not
landed:

* **The frame pointer cannot be cached on `frame_idx` alone, and caching it
  on `(frame_idx, reloc_epoch)` costs what it saves.** `frame_idx` is not the
  only thing that invalidates `hot_fp`: an `invokestatic` of a native that
  calls back into Java pushes frames on the SAME thread beyond the current
  one, and a `FrameStack` growth there relocates the buffer while `frame_idx`
  is unchanged. So the cache needs a per-iteration `reloc_epoch()` load and
  compare — about the same work as today's `frame_ptr` (a length compare, a
  base load and an index multiply).
* **`code_ptr` / `code_len` are already frame-switch-keyed in effect.** The
  per-iteration `code_ptr` load IS the frame-switch detector (wave 2); the
  only thing left to cache is `code_len`, one load from the same cache line.
* The loop top gained one hoisted-bool test this wave (`breakpoints_armed`,
  see `docs/internal/fixed-bugs/interpreter-L1-breakpoints-are-never-checked-by-the-dispatch-loop-FIXED-20260925.md`),
  and the per-branch branch-profile read moved to frame switches and back
  edges (`docs/internal/fixed-bugs/interpreter-L1-proposal-branch-profile-arm-per-frame-switch-FIXED-20260923.md`) —
  both in the direction of Stage 1's "state in loop locals, refreshed at
  switches".

So Stage 1 is a measurement question, not a code question: do it only with
Stage 0's numbers, and together with Stage 4 (pc/sp in locals), which is the
stage that actually removes per-bytecode memory traffic. Stage 2's
precondition (every method reaching the fast path has proven maps) is still
unmet.

## Progress (wave 4)

**Stage 0, the software half, is in.** `CRATONVM_QUICKEN_STATS=pairs` (a new
value of the existing flag, so no new flag was registered; any other value
keeps meaning "footprint report") arms an opcode-pair census:

* The dispatch loop hoists `pair_census_enabled()` once per entry and folds
  it with the debugger gate into one `loop_top_hooks` bool, so the unarmed
  loop tests exactly what it tested before (the one `breakpoints_armed` test
  became the `loop_top_hooks` test).
* Armed, every fast-path dispatch goes through the cold `note_opcode_pair`
  (`vm/src/runtime/interpreter.rs`), which counts `(previous, current)` only
  when the current pc is the previous instruction's fall-through successor in
  the same activation (same `frame_idx` and bytecode allocation; the length
  comes from `cratonvm_reader::quickened::fixed_instruction_length`). A call,
  return, taken branch or unwind breaks the pair, so what is counted is
  exactly the set a superinstruction could fuse. Fused groups dispatch once,
  so the census ranks what STILL dispatches separately on this build — the
  ranking the next fusion should be chosen from. Decoded-path dispatches
  (vetoed methods, `--noverify`) are not counted.
* Counts are process-wide relaxed atomics (`PAIR_COUNTS`, 64 Ki slots in
  `reader/src/quickened.rs`); `report_opcode_pair_census` prints the top 60
  with mnemonics and shares at exit (`interp_census::report_at_exit`, one
  line added there).

Tests: `quickened::tests::{fixed_instruction_length_matches_decode,
opcode_pair_census_counts_and_ranks}`,
`l1_dispatch_tests::the_opcode_pair_census_counts_fall_through_pairs_only`.

Not done: the hardware half of Stage 0 (`perf stat -e
branches,branch-misses` on a Linux host against HotSpot `-Xint`), triples
(a 16 Mi-slot table was not worth it before the pair ranking exists), and
every later stage. Suggested first use: run the Spring Boot / Tomcat / ES
suites and CratonBench under `CRATONVM_QUICKEN_STATS=pairs --nojit` and
attach the top-60 tables to the superinstruction page.

## Progress (wave 5)

No stage landed: every remaining stage is a per-bytecode cost claim and this
wave could not run a binary, while the one measurement it received (the
orchestrator's `L1Wave4Fusions` interleaved runs: `arrayLoop` ~12% slower on
the wave-4 build, `sumLocals` flat) pointed at loop-carried state rather than
at a stage. What was found and changed:

* **The JDWP gate is live in a workspace build.** `libcratonvm` enables
  `experimental-debug` on `cratonvm-vm` unconditionally, and Cargo unifies
  features across every package a `cargo build` at the workspace root
  selects (there is no `default-members`), so the `cratonvm` binary built
  that way carries the debugger surface: `breakpoints_armed_now` is a real
  load, not the constant `false` a `-p cratonvm-cli` build folds it to. Any
  per-bytecode or per-back-edge figure should say which build it came from.
* **`refresh_debugger_gate!` compares before it writes.** Since wave 4 it ran
  at every backward branch and stored `breakpoints_armed`, `fusion_off` and
  `loop_top_hooks` unconditionally — three stores into the dispatch loop's
  stack slots per back edge. `fusion_off` and `loop_top_hooks` are pure
  functions of `breakpoints_armed` and two entry-time constants, so the
  macro now loads the gate, compares it with the local and writes only on a
  change: the same answers, one load / compare / predicted branch per back
  edge.
* The array-loop test now fuses into one dispatch (see
  `i1-L1-proposal-superinstruction-coverage-20260923.md`, Progress (wave 5)),
  which removes two whole preamble iterations per trip of javac's array loop
  — the largest per-iteration saving available without touching the
  preamble itself.

Next stage to try, with the measurement it needs: Stage 4 (`pc` in a local,
written back at escape points) remains the only stage with a claim to the
~8x straight-line ratio; take `perf stat` branch-miss numbers first (Stage 0's
hardware half), on a `-p cratonvm-cli` build.

## Progress (wave 6)

No stage landed (each is still a per-bytecode cost claim). One precondition
moved:

* **Stage 2's precondition, the locals half, is met.** Every method that
  reaches the fast path now has its local indices proven: verified methods by
  `safe_for_fast_path`, and methods with no maps by the new once-per-method
  `unproven_locals_fit` (`cratonvm_reader::quickened::fast_path_local_reach`,
  memoized with the rest of the admission; see
  `docs/internal/fixed-bugs/interpreter-L1-short-form-local-opcodes-skip-the-max-locals-guard-FIXED-20260924.md`).
  So raw-pointer LOCAL access (the `get_local_*_unchecked` family) would
  already be sound, apart from a stale memo entry, which the H7 guards cover
  for explicit indices only. The STACK half is not: a no-maps method's
  `max_stack` is still unproven, and raw-pointer stack access would turn its
  bounded `Vec` panic into memory unsafety. The same walk can carry an
  abstract stack-depth pass (it needs each invoke / field descriptor's slot
  count, i.e. constant-pool access the reader-side walk does not have — pass
  a callback) before Stage 2 is attempted.
* The debugger refresh (`refresh_debugger_gate!`) now also re-reads the JVMTI
  single-step union flag: one more load of a static per back edge and per
  frame switch. Include it in any back-edge cost figure.

## Progress (wave 7)

**Stage 2's precondition, the stack half, has its proof and its cache; no
arm uses it yet.**

* `cratonvm_reader::quickened::fast_path_reach(code, handlers, cp)` (new;
  `fast_path_local_reach` is now its locals-only wrapper) proves, in the same
  decode as the locals bound, the deepest the operand stack can get in JVM
  words: an abstract interpretation from depth 0 at pc 0 and depth 1 at each
  handler, per-opcode effects with field / method / `invokedynamic`
  descriptors read from the class's constant pool, along fall-through,
  branch and switch edges. It refuses (`stack: None`, never a guess) an
  underflow, a join at two depths, an unresolvable member, code that falls
  off its end, and `jsr` / `ret`. Words bound slots: the interpreter keeps a
  `long` / `double` in one slot. Test:
  `quickened::tests::stack_reach_is_proven_in_words_or_refused`.
* `vm/src/runtime/interpreter.rs`: the admission memo (`FAST_PATH_MEMO`)
  gained a third flag, `FAST_PATH_MEMO_STACK_PROVEN` (the key moved up one
  bit), set for a method with verifier maps that prove it
  (`safe_for_fast_path`) and for a map-less method whose code
  `fast_path_reach` proves within `max_stack` (`unproven_code_fits`, which
  replaced `unproven_locals_fit`; the pool comes from `frame.class_id`'s class
  under `try_read`, an empty pool standing in when the class is absent or the
  lock busy). `frame_stack_depth_proven(shared, frame)` reads it; nothing on
  the dispatch path does yet. `CRATONVM_QUICKEN_STATS=1` now also prints a
  `stack-unproven` line per admitted map-less method the pass could not
  prove: run it over CDS-heavy workloads to size what Stage 2 would have to
  send to the decoded path. Test:
  `l1_dispatch_tests::map_less_stack_depth_is_proven_in_the_admission_walk`.
* What Stage 2 still needs before it can rely on the bit: a stale memo entry
  (the allocation reuse case `FAST_PATH_MEMO`'s doc describes) could hand a
  proven verdict to another method's code; the raw arms need either a key
  that cannot collide (e.g. the verdict stored on `CachedBytecodeMethod`) or
  a `max_stack` equality check folded into the entry.
* The debugger gate is now per method (see
  `docs/internal/fixed-bugs/interpreter-L1-breakpoints-are-never-checked-by-the-dispatch-loop-FIXED-20260925.md`,
  Progress (wave 7)): while a JDWP breakpoint is set, a back edge in an
  unchanged session costs one more load (the summary's generation), and a
  method without a breakpoint keeps its fusions.

## Wave 22 note (lane L7)

No stage of this plan landed; wave 22 went after fixed per-execution costs
that sit INSIDE arms rather than in the preamble, because they are local,
measurable with a timing probe, and do not depend on Stage 0's numbers:

* **`aastore` had no quickened half.** Every reference-array store paid two
  operand decodes, four `VmHeap` enum dispatches and `set_array_element`'s
  re-validation. `field_fast::array_store_ref` now serves a null or plain
  reference into an in-range reference array when the lock-free covariance
  verdicts confirm it, with `set_array_element`'s barrier order; A/B rows in
  `tools/probes/interp/L7/L7W22AastoreBench.java`.
* **Instrument gates re-read per boundary.** `getfield_fast_keyed` (every
  quickened `getfield`) re-read the `CRATONVM_DBG_FIELD_PHASES` gate byte at
  each of eleven phase boundaries — about a dozen loads per unarmed field
  read. It now reads it once (`field_phases::now_if` / `charge_if`); rows in
  `tools/probes/interp/L7/L7W22FieldReadBench.java`. The same shape remains
  in other lanes' files: the `invoke_phases::now()` / `charge()` boundaries
  in `dispatch_static.rs` (four `now` + three `charge` on the static
  dispatch path) and `jvmti_events.rs`'s frame-push path (six), each an
  independent gate load per call. The fix is the same: read
  `invoke_phases::on()` once into a local and pass it down.
* Still true of the preamble: items 1-9 of "What one iteration of the fast
  path does today" are unchanged; wave 22 re-read them and found nothing new
  that is removable without Stage 1/4's escape-point discipline.

**What to do next, and why.** Of the six interpreter-performance proposals
this round keeps (this one, superinstruction coverage, split `execute`,
static-field quickening, contiguous stack, refill-driven GC poll), the next
step is not code: it is Stage 0's hardware half, which has never run —
`perf stat -e cycles,instructions,branches,branch-misses` over
`tools/probes/interp/L1/L1FusionBurn.java`, `L1SlotCopyBurn.java` and
`L7/L7W22FieldReadBench.java` under `--nojit` on a `-p cratonvm-cli` build,
next to HotSpot `-Xint`, plus one `CRATONVM_QUICKEN_STATS=pairs` run of the
Spring Boot and Tomcat samples. Those two numbers decide between the two
code directions: a high branch-miss rate points at Stage 4 (pc/sp in
locals, then threaded dispatch); a low one with a concentrated pair census
points at more fusions (`i1-L1-proposal-superinstruction-coverage`), which
are local and have landed cleanly five times. Without the numbers, the
refill-driven GC poll's kill switch is the one change with a finished
measurement plan (its page, stage 1), and it should be measured and the
switch deleted first.

## Wave 26 note (lane L7)

No stage of this plan landed; the preamble changed underneath it:

* **Item 3 is now the thread's own poll word.** The `stw_flag.load(Acquire)`
  and the wave-23/24 `code_moves` compare are one relaxed load of
  `threading::gc_barrier::LoopPollWord` and one compare against a register
  (`069cbb656`; see
  `i24-L3-proposal-one-per-thread-poll-word-for-safepoints-and-frame-moves-20260927.md`,
  "Wave 26 note"). The aarch64 caveat the in-code note carried (an `ldar`
  per bytecode) is gone with it. Back edges no longer load the flag at all
  (`4730377b8`).
* The preamble's per-iteration work is otherwise items 1, 2 and 4-12 as
  listed above, unchanged. Stage 1 (frame-switch caching) remains a
  measurement question for the reason wave 3 gave; Stage 2's stack half
  still waits on a collision-proof memo key; Stage 4 still needs Stage 0's
  hardware numbers. Take those on a build that has the four wave-26 L7
  commits (`i25-L7-the-interpreter-dispatch-loop-is-slower-than-on-wave-23-20260927.md`,
  "Progress (wave 26)"), or the baseline moves under the measurement.

## Wave 27 note (lane L7)

No stage landed; three findings bear on it (details on
`i25-L7-the-interpreter-dispatch-loop-is-slower-than-on-wave-23-20260927.md`,
"Progress (wave 27)").

* **A borrow anywhere in the function puts a local in memory everywhere.**
  Item 8's `opcode` was stored to a stack slot on every bytecode, and
  `saved_pc` reloaded at every loop top, because diagnostics formatted them
  in place: a `format_args!` argument is a borrow, rustc gives a borrowed
  local a stack slot for the whole function, and the escape into the
  formatter stops LLVM promoting it back. Fixed for `opcode`, `saved_pc`,
  `b1`, `b2` and the return arm's `value` (`d0ca42cec`, `af61a55e6`); the
  rule is documented on `value_return_underflow`. It is a precondition of
  Stage 4 (pc/sp in locals): a local that is ever borrowed never stays in a
  register, so every write-back-at-escape-points design must also keep its
  locals unborrowed.
* **Item 9 is the profile's heaviest instruction.** On both wave-26 fat-LTO
  builds, `add 0x1e8(%rax),%r9` (the frame-array base for
  `&mut thread.frames[frame_idx]`) carries 7.3% of the loop's samples, the
  load chain `thread` (spilled) -> `frames` base -> frame that `hot_fp` already
  walked at the loop top. Stage 1 in its smallest form: a `FrameStack`
  accessor `fn reborrow_hot(&mut self, p: *mut Frame, idx: usize) -> &mut
  Frame` that debug-asserts `p == frame_ptr(idx)` and hands `p` back with the
  lifetime of `&mut self`, so the arms' `let _ = frame;` discipline still
  compiles only when sound. Sound only while nothing between the derivation
  and the use can push a frame: the `code_ptr != fast_gate_code_ptr` block is
  read-only, but the loop-top hooks (`deliver_breakpoint_if_set`,
  `fire_jvmti_single_step`) can run Java through an agent and must re-derive
  `hot_fp`. `FrameStack` is lane L3's file.
* **Stage 0's hardware half is now the cheapest way to explain a measured
  regression.** The wave-26 "layout step" is ~45 ns per `TypeCheckBench`
  `classMono` iteration between builds whose loop-top code is the same
  instruction for instruction in the excerpts compared; `perf stat -e
  cycles,instructions,branch-misses,br_misp_retired.indirect` on the two
  builds says whether it is the single dispatch site's prediction (Stage 6's
  question) or the arms' code (wave 27's outlining).
