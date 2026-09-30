# Proposal: make a withdrawn compiled body not entrant

**Status: stages 1-3 landed for x86-64 (wave 22, lane L6); wave 24 (lane L6) closed the scan-to-eviction window, skips or coalesces the loop-exit pause, re-checks direct-bind gates on a forward and batches the patch's protection changes; the caller
re-bind (wave 22 note) landed as stub forwarding in wave 23 (lane L6); stage
4's non-redefinition withdrawals and stage 5 open; aarch64 not applicable
(its bodies bake no other body's address) — filed 2026-09-25 by interpreter
round i1 wave 21, lane L1.**

## Why

A compiled body withdrawn from the `JitCache` (a redefinition, a
de-speculation eviction, a sweep) stays mapped as long as anything baked its
address, and every baked caller keeps entering it:

* plain direct CALLs of both tiers (`_direct_callee_entries`, the IR tier's
  `ir_direct_callee_entries`), including instance-site binds;
* spliced lambda adapters and cycle-edge cells are cleared by the closure and
  the cell passes, and in-loop / OSR `invokestatic` sites read a RETIRE cell
  (round 9 wave 9; wave 21 extended it to every site of an OSR body), which
  costs a `MOV r11, imm64`, a load, a test and a taken branch on every call.

For a redefinition that is a correctness gap
(`docs/internal/fixed-bugs/interpreter-L6-baked-calls-outside-retire-cells-reach-a-redefined-callees-old-body-FIXED-20260926.md`):
a running caller's next call runs the old bytecode. For an eviction it is the
slowdown the retire cells were built for. HotSpot answers both the same way:
`nmethod::make_not_entrant` patches the verified entry point with a jump to
`SharedRuntime::get_handle_wrong_method_stub()`, which re-resolves the call
from the caller's call site; callers pay nothing until then.

## Design

1. **A patchable entry.** Both backends start every body with one instruction
   of at least 5 bytes at an 8-byte-aligned entry (a 5-byte `NOP` where the
   prologue's first instruction is shorter), so one aligned 8-byte store can
   replace it with `JMP rel32` atomically with respect to a thread that is
   about to execute it, and no thread can be stopped INSIDE it (a suspended
   thread's RIP is at an instruction boundary).
2. **The not-entrant stub**, one per VM, reached by that `JMP`: it calls
   `jit_note_not_entrant_entry` (sets a thread-local "the callee refused this
   entry" flag), loads `i64::MIN` into RAX and returns. The callee built no
   frame and touched no callee-saved register, so the caller's state is
   exactly as at the CALL.
3. **The caller's service.** Every baked call site already has the
   callee-deopt service (`emit_inline_callee_deopt_check` →
   `jit_service_callee_deopt` → `handle_compiled_callee_deopt_sentinel`, and
   the IR tier's `emit_inline_callee_deopt_service`), with the site's
   `JitInvokeInfo` and the arguments in the service range. A first arm there
   takes the flag and re-dispatches the call through the interpreter's invoke
   path by `info` (`bail_to_interpreter`, as `rerun_declined_callee_from_entry`
   does for a declined stash), which reaches the newest body or the new
   bytecode. A site without the service (`dbg_unserviced_direct_call`) must
   get one, or its body must not be patched (then its caller is withdrawn as
   today).
4. **When to patch.** At the redefinition's pause (the `redefinition`
   handshake, `obsolete_frames::after_redefinition`; take it for every
   redefinition that withdrew a body, not only when constants moved) for the
   bodies `JitCache::invalidate_for_redefinition` withdrew plus the
   superseded ones `redefinition_call_dependents` reached. The code page is
   RX (`platform::make_executable`); the patch needs a write window: a dual
   (RW alias) mapping of the code arena, or an RWX flip of the one page under
   the pause (a documented W^X exception, like the `JitWriteScope` toggle on
   Apple silicon). A thread that loaded the old entry and is between the load
   and the CALL of a cell or MIC is the existing race; the stub handles it
   the same way.

## Expected benefit

* Correct calls after a redefinition from every baked shape of both tiers
  (shapes 1-3 of the i21 page), with no per-call cost.
* Stage 5 below: the retire cells can go, which removes their per-call load /
  test / branch from in-loop static calls, and the eviction's "the caller is
  retired too" closure can keep callers whose callee was merely evicted.

## Staged plan

1. Entry shape in both backends + a jit-crate test that every published body's
   first 8 bytes start with a ≥ 5-byte instruction at an 8-aligned address
   (code-size census before/after on the artifact corpus,
   `jit/tests/x64_artifact_corpus.rs`).
2. The stub, the flag, the service arm; a unit test that calls a patched body
   directly (as `jit/src/tests.rs` calls bodies) and gets the re-dispatch.
3. Patch on redefinition under the pause; probe: the i21 page's method-entry
   variant of `tools/probes/interp/L7/RedefineCompiledOldConstantsProbe.java`.
4. Patch on every withdrawal (eviction, sweep).
5. Retire the retire cells (`retirable_direct_call`,
   `JitCycleEdgeCell::new_retire`, `CRATONVM_JIT_RETIRE_CELL`) once 4 holds;
   measure `TryLoop throw` (the cells' own benchmark) and `CratonBenchC2`.

## Risk

High for stage 3 (writing executable memory while other threads run; W^X);
stages 1-2 are local. A missing service at one site turns a not-entrant entry
into a caller replay, so stage 3 must refuse to patch a body with an
unserviced caller.

## Progress (wave 22) — lane L6

Stages 1-3 landed for x86-64, with one design change; the record is
`docs/internal/fixed-bugs/interpreter-L6-baked-calls-outside-retire-cells-reach-a-redefined-callees-old-body-FIXED-20260926.md`
and the code `jit/src/not_entrant.rs`.

* **Stage 1.** Both x86-64 tiers open every method-entry body with
  `ENTRY_PATCH_PAD` (a five-byte `NOP`) at the page-aligned buffer start. The
  stub does not live in a shared per-VM page: every body has its own mapping,
  which need not be within `rel32` of any shared stub, so every
  `ExecutableBuffer` maps a 128-byte tail past its emit capacity
  (`NOT_ENTRANT_TAIL_RESERVE`) and the stub is written there at patch time.
* **Stage 2, changed.** The stub is a c2i adapter, not a "refused" sentinel:
  it spills the ABI argument registers and calls
  `jit_not_entrant_entry(record, regs, caller_stack_args)`, which re-dispatches
  the call by class id through the interpreter's invoke path and answers in
  the compiled ABI. That removes the design's dependency on every caller having
  the callee-deopt service (the "refuse to patch a body with an unserviced
  caller" rule of the Risk section is unnecessary): IR `sync_exit` direct
  calls, lambda adapters, Rust doors and self-calls are all answered correctly.
  The stub builds no frame and leaves RBP and the frame mirror alone, so GC
  sees the helper as called from the caller's call site; its spill area is in
  the JIT band the young-pin deposit counts, like every Rust helper frame.
* **Stage 3.** On every redefinition (scoped eviction and full flush) for the
  bodies stale by their own record (declared, inlined, or labelled with the
  class), published or reachable only through baked callee roots; patched
  after the eviction, under the cache's mutation lock, RWX only for the
  patch.

Open: stage 4 (patch on every withdrawal -- a de-speculation eviction or a
sweep also leaves baked callers entering the old body; with a c2i stub this is
now a correctness-neutral perf question, since the re-dispatch reaches the
newest body); stage 5 (retire the retire cells: needs stage 4 and the rebind
below, else an in-loop caller that used to re-read its cell pays the VM
re-dispatch on every call); aarch64.

## Wave 22 note: re-bind the caller

HotSpot's wrong-method stub re-resolves the CALLER's call site and patches it
to the new target, so a running loop pays the re-dispatch once. Here a caller
keeps its `CALL rel32` into the patched entry and pays the full VM
re-dispatch (a helper call, argument decoding, an interpreter invocation that
may in turn enter the new compiled body) on every call until the caller
itself is replaced. Measure with `RedefineScopedEvictionBench`-style probes: a
running OSR loop calling a redefined static method, ns/call before and after
the redefinition, JIT on. A re-bind would record the caller's call-site
offset in the record's reach (the return address minus 5 identifies an
`E8 rel32`), and patch the rel32 of a 4-aligned site the way the entry is
patched; it needs the emitters to align direct `CALL` displacements, so it is
priced against the code-size census first.

## Progress (wave 23) — lane L6

**The re-bind landed, on the stub instead of the caller.** Patching the
caller's `CALL rel32` was rejected: the displacement is not 4-aligned (only
5 of 8 sites could be patched with one aligned store), the caller's own
mapping would need a W^X window while its other threads run it, and the
shapes that reach a patched entry without an `E8` (a Rust door, a stale
inline-cache word, a retire cell) would stay slow. Instead every stub opens
with a 41-byte forwarding prefix (`not_entrant_stub_bytes`, x86-64): it reads
the record's forward word and, while the named body is neither `retired` nor
`superseded`, `JMP`s to its entry with the caller's registers, stack and
return address untouched. The VM's helper fills the word on its first slow
call (`NotEntrantRecord::forward_to`) with the body now published under the
record's key, admitted only if a direct call of the same callers could have
baked it (same key, context ABI and arity; not wrapped-entry, not an indy
trap, not itself retired, superseded or not entrant) and only if it does not
reach the patched body through baked roots or forwards (a strong-reference
cycle); at most `NOT_ENTRANT_MAX_FORWARDS` (8) per record, all kept for the
record's life. Kill switch: the `const` `NOT_ENTRANT_FORWARDING_ENABLED`.
Cost per forwarded call: the patched `JMP`, one load, two byte tests and an
indirect `JMP`. Measure: `tools/probes/interp/L6/NotEntrantCallBench.java`,
`after` row, wave-22 vs wave-23 build (header states the expected direction).
The stub grew to at most 112 bytes (`NOT_ENTRANT_STUB_MAX`), which still fits
the 128-byte tail of a buffer emitted to its last byte
(`not_entrant::tests::the_forwarding_stub_fits_the_tail_of_a_full_buffer`).

Three consequences had to be closed with it:

* **The GC's frame identity.** A forwarded call builds the TARGET's frame
  under a caller whose `CALL` names the patched body. The root walk's
  `innermost_frame_method` (`vm/src/jit/conservative_roots.rs`) used to trust
  the chain entry's body for a non-JIT caller or a direct self-call, and
  `direct_call_callee` the decoded `CALL` target; for a not-entrant body both
  now answer only from the identity the frame's own prologue published, else
  fail closed (the non-moving sweep), never with the patched body's map.
* **The callee-deopt sink.** Before forwarding, a trap in the new body came
  back through the interpreter door's sink, which resumes a fresh body of a
  redefined class. Through a forward it reaches the call site's service, whose
  `try_resume_trapped_callee` refused EVERY stash of a class ever redefined and
  re-ran the callee from entry, replaying what it had committed. It now
  resumes a stash taken in the body currently published for the key and
  compiled after the class's last redefinition
  (`helpers.rs::stash_is_from_a_current_body`) -- which also fixes that replay
  for any compiled caller baked after a retransform, forwarding or not.
* **Profilers.** The stub is published to the perf map and jitdump as
  `not-entrant-stub <label>` at patch time.

**Whole-cache flushes patch every body.** A redefinition that flushes the
whole cache (the class was spliced or inlined without a per-body record)
used to patch only the bodies stale by record, so a baked caller kept
entering a flushed IR body that had spliced the class. It now patches every
body ever published under a method key (HotSpot deoptimizes all nmethods when
dependencies are not recorded). Cost: two `mprotect`s per body under the
cache's mutation lock, once per such redefinition.

**Refusals on a real workload (item 3 of the wave-23 brief).** Every pass now
prints one `[cratonvm-jitc] not-entrant pass: candidates=.. patched=..
Published=.. Unsupported=.. NoEntryPad=.. Unaligned=.. NoIdentity=..
ArityMismatch=.. NoTail=.. Protect=..` line under `CRATONVM_DBG_JITC=1`
(per-reason counters in `JitCache`). By construction, on x86-64:
`Published` counts only bodies of a SAME-NAMED class of another loader (the
label clause of `stale_by_record` matches by name, the scoped eviction by id;
an inherited body published under a receiver's key marks the declaring class
copied, which takes the whole-cache path) -- correct refusals, zero outside
multi-loader applications; `NoIdentity` / `ArityMismatch` / `NoEntryPad` /
`Unaligned` need a published method body not produced by the two x86-64 tiers
or not stamped by `try_compile` / the single-pass driver, which no production
door publishes, and the whole-cache walk skips never-published artifacts
(lambda adapters); `NoTail` cannot happen (the tail fits the largest stub);
`Protect` only on an `mprotect` refusal (`ENOMEM` at `vm.max_map_count`, which
needs `CRATONVM_JIT_CODE_ARENA`'s split VMAs). So the gap is negligible; the
orchestrator's run of the L6 agent probes with `CRATONVM_DBG_JITC=1` confirms
it with numbers.

Still open: stage 4 for the non-redefinition withdrawals (a de-speculation
eviction, the cold sweep, a CHA invalidation -- CHA binds are guarded, so this
is performance, not correctness: a caller of such a body keeps running the
withdrawn but still correct code); stage 5 (retire the retire cells), which
now needs only stage 4, since forwarding removed the per-call re-dispatch that
made the cells cheaper. aarch64 is not applicable
(`docs/internal/fixed-bugs/interpreter-L6-a-running-splice-of-a-redefined-callee-runs-its-old-bytecode-FIXED-20260926.md`,
item 3).

## Wave 23 note: patch without an `mprotect` pair per body

Each patch is two protection changes of the body's own mapping
(`platform::make_code_patchable`, then `make_executable`), under the cache's
mutation lock; the second removes a permission, so on Linux it is a TLB
shootdown to every core running the process. Since wave 23 a whole-cache
redefinition patches EVERY body, so a cache of N bodies costs 2N system calls
and N shootdowns while no compile can publish (a guess to be measured: a few
microseconds each, so 20 000 bodies would hold the lock for tens to hundreds
of milliseconds). Stage 4 multiplies the number of patches again.

Design: map every code buffer twice -- the executable view and a writable
alias of the same pages (`memfd_create` + two `mmap`s on Linux, a section
object with two views on Windows, `MAP_JIT` + `pthread_jit_write_protect_np`
already serves Apple silicon) -- and patch through the alias: no protection
change, no shootdown, W^X kept (no page is ever writable and executable in the
same view). The arena (`CRATONVM_JIT_CODE_ARENA`) is the natural place: one
alias per region, not per body. Cheaper interim step: batch the pass by region
when the arena is on (one `mprotect` pair per region whose blocks are being
patched).

Measure: a redefinition of `java/lang/Object`-dependent code (any whole-cache
flush) in a warmed Spring Boot app with `CRATONVM_DBG_JITC=1`: the pass's
`candidates=` count, and the `redefine_class_with` wall time before and after.
Risk: medium (the alias doubles the address-space footprint and every code
write path must choose the right view).

## Progress (wave 24) — lane L6

The three risks wave 23 left unmeasured, and one it did not see:

* **The loop-exit handshake per redefinition.** `request_withdrawn_body_exits`
  now returns without a pause when no other thread holds a JIT entry and no
  OSR body is starting (`conservative_roots::peer_threads_hold_jit_entries`,
  `jit_bridge::osr_bodies_running`); the OSR door counts its body running
  BEFORE it asks whether that body was withdrawn and refuses to start one
  (`try_osr`), so a body entered after the skip cannot be a withdrawn one.
  And `redefine_class_with` takes no second pause when the redefinition's own
  handshake (`obsolete_frames::after_redefinition`, taken when constants
  moved) already made every peer poll without freezing any. A batch of
  retransforms from an agent thread while the application waits in
  interpreted code or native calls costs no pause at all; one while compiled
  loops run still costs one pause per class (coalescing across the classes
  of ONE `RetransformClasses` call needs the JVMTI layer to defer it to the
  end of the batch -- not done).
* **Forwards and late direct-bind gates.** `admits_forward_target` now
  refuses a static GPU kernel (`offload_hook::keeps_dispatch_helper`, what
  both tiers' direct-call planning asks), and the VM's helper refuses to
  forward to a method that must stay on dispatch because an inline cache
  closed a recursion cycle through it
  (`JitVerdictRegistry::direct_call_requires_dispatch_of`): the forward
  would otherwise make the forwarded caller bind directly what a caller
  compiled now could not. The forward's cycle check
  (`reaches_artifact`) already keeps the baked-edge graph acyclic, which the
  missing stack-depth guard on a direct call relies on. An agent's
  interpreter-only mode needs nothing: a forwarded call reaches the target's
  entry, whose polls leave as a baked call's would.
* **Two `mprotect`s per body.** The patch is prepared for every body first,
  then the protection of each run of page-adjacent mappings is changed once
  (`adjacent_patch_runs`; bodies compiled in a row are usually adjacent
  anonymous mappings), with a per-body fallback when a run's change is
  refused; never across allocations on Windows. The writable-alias design of
  the wave-23 note remains the way to drop the protection changes entirely.
  Measure: `CRATONVM_DBG_JITC=1`'s `not-entrant pass:` line (candidates) and
  the wall time of `redefine_class_with` on a whole-cache redefinition in a
  warm application, before and after.
* **Not seen before: the scan-to-eviction window.** A compile of the old
  bytecode could publish between `redefinition_stale_bodies` and the
  eviction: withdrawn, never patched. The redefinition now fences
  publications first (`JitCache::fence_redefinition_publications`).

`NotEntrantCallBench`'s `after` row never measured the stub (its `after` loop
was compiled after the retransform); it now times one loop across it. By
design the forward costs about 11 predictable instructions per call over a
direct call (header of the bench). The two flag tests are the price of
re-learning: without them a forward to a body that was later superseded
(C1 by C2) or evicted would keep going there. Stage 4 (every withdrawal
patched) would make the `retired` test redundant -- a withdrawn target
would be not entrant itself -- and a supersede could clear the forward word
of the records forwarding to the superseded body; neither is worth doing
before the bench's `after - before` is measured above ~3 ns.

## Progress (wave 25) — lane L6

* **Forwarding prefix: one flag test.** When a `CompiledMethod`'s
  `superseded` flag is the byte after its `retired` flag (asked of a live
  body at patch time, `not_entrant.rs::retired_and_superseded_adjacent`; the
  layout of a `repr(Rust)` struct is not promised), the stub tests both with
  one `CMP WORD [R10], 0`: 8 instructions / 32 bytes instead of 11 / 41
  (`not_entrant_stub_bytes`, `flags_adjacent`). Measure on
  `NotEntrantCallBench`'s `after` row, wave-24 against wave-25 build.
* **What is left of the forward's cost** is the patched `JMP`, the forward
  word's load and the indirect `JMP`. Only re-binding the caller removes
  them, and the two ways considered both need more than this wave could
  verify without a build: (a) patching the caller's `CALL rel32` from the
  VM's slow path (`NotEntrantRecord::caller_return_address` names it) needs
  a 4-aligned displacement or a two-step patch, a protection window on the
  caller's mapping, and -- the real obstacle -- the caller's
  `_direct_callee_roots` must gain the new target, or a later class-change
  invalidation of that target would not reach the caller through the
  direct-call closure; (b) re-pointing the patched ENTRY's `JMP` at the
  target directly needs a reverse index from a target to the records
  forwarding to it, re-patched on the target's retire and supersede. Both
  are the i15-L3-independent half; the poll naming its body (landed in wave
  17) does not bear on the call path.
* **The loop-exit handshake per `retransformClasses` call**, not per
  redefinition: `instrument.rs`' `retransformClasses0` / `redefineClasses0`
  open a batch (`NativeClassAccess::begin_redefinition_batch`, per thread,
  `JvmThread::redefinition_batch`), each `redefine_class_with` in it defers
  its handshake, and the batch's end takes one if any body was withdrawn.
  A retransform of N classes redefines each twice (original bytes, then the
  transformed ones), so it cost up to 2N pauses.
* **Frames in a call at the redefinition** leave at their next exit-capable
  back edge: the redefinition forces the exit polls of every body it
  withdrew (`JitCache::force_withdrawn_exit_polls`,
  `i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md`).

## Wave 26 note — lane L6

* **A redefinition now reaches superseded bodies that are still alive.**
  `JitCache::redefinition_stale_bodies` walked the maps and the baked callee
  roots only, so a body superseded by a tier-up while a frame kept running it
  (and reachable from nothing baked) was neither made not entrant nor, on a
  scoped redefinition, even withdrawn. Every body with exit-poll sites is now
  registered weakly (`JitCache::note_exit_poll_body`) and the scan adds the live
  published ones
  (`i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md`,
  "Progress (wave 26)"). This is stage 3 (redefinition) widened, not stage 4.
* **Stage 4 (patch on every withdrawal) is still not taken**, for the reason
  wave 23 gave: with the c2i stub it is a performance question (a caller of a
  de-speculated, swept or CHA-invalidated body keeps running correct code),
  and nothing has measured the retire cells' per-call cost it would let stage 5
  remove. Measure first: `TryLoop throw` and `CratonBenchC2` with
  `CRATONVM_JIT_RETIRE_CELL=0` against the default, on a fat-LTO build; only a
  visible gap pays for patching executable memory on every eviction.

## Wave 28 note — lane L6

Stages 4-5 still not taken: nothing here is sound to land without the A/B
wave 27 named (`TryLoop throw`, `CratonBenchC2` with
`CRATONVM_JIT_RETIRE_CELL=0` against the default on a fat-LTO build), and the
lane does not build. Round 12 changed the ground under stage 4 in one way worth
recording: every withdrawal now demotes its methods through the cache itself
(`JitCache::install_withdrawal_sink` / `report_withdrawn`, W3-1), so a
stage-4 patch on an eviction or a sweep would no longer have to demote by
hand -- only arm the not-entrant hook at VM start (fact (a) below) and patch.
Reviewed against the retirement invariants (forced exit polls, the weak
exit-poll registry, bodies spared as obsolete, redefinition withdrawal): the
sink runs after the cache's mutation lock is released and after the flush's
install barrier is taken, so `note_whole_cache_redefinition` still stamps the
same barrier, and `JitRealm::demote_withdrawn` becoming a no-op loses nothing
on the scoped path (`invalidate_for_redefinition` withdraws through
`invalidate_matching_collecting`, which reports). The one semantic change: a
scoped redefinition that withdraws only an OSR body no longer demotes its
method (`report_withdrawn` passes method-entry keys only), which is what
`current_tier` means; the OSR door's own withdrawn-body refusal and the
code-state epoch still retire the OSR body.

## Wave 27 note — lane L6

* **Stages 4-5 still not taken**: the measurement wave 26 asked for needs a
  build, which this lane does not run. Two facts for whoever takes stage 4,
  read off the code: (a) the patch hook is armed only by the VM's FIRST class
  redefinition (`vm_exec.rs::redefine_class_with` calls
  `JitCache::arm_not_entrant`), so a stage-4 patch on a de-speculation
  eviction or a sweep before any redefinition would patch nothing -- the hook
  has to be armed at VM start (it needs only the `SharedVm` address and the
  helper, both known then); (b) a withdrawal outside a redefinition leaves
  the withdrawn body's code CORRECT, so a stage-4 patch buys only the
  retirement a retire cell buys today, and stage 5's win is the cell's
  per-call load, test and branch on in-loop `invokestatic` direct calls
  against the forward's patched `JMP`, load, flag test and indirect `JMP` on
  the first call after each withdrawal. That is the A/B to run.
* **Post-call exits** (wave 27): the redefinition's force pass now also
  rewrites a single-pass OSR body's post-call exit sites, so a frame inside a
  call at the redefinition leaves at the call's successor
  (`i26-L6-proposal-a-patchable-post-call-exit-for-withdrawn-bodies-20260928.md`,
  "Progress (wave 27)"). Like the forced polls it touches only withdrawn
  bodies, which no new activation enters.
