# A megamorphic `invokeinterface` still pays the Rust helper in six shapes

Status: OPEN
Area: JIT calls (inline caches, hashed stub, shared megamorphic table)
Severity: MEDIUM (performance; no wrong answer)
Found by: round 12 wave 3 lane mega2

## What landed first (the baseline this page reads against)

Round 12 wave 3 put the VM's shared megamorphic table
(`inline_cache_pic.rs::MegaDispatchTable`) into machine code: the hashed stub
(`runtime_lowering.rs::emit_hashed_vtable_stub_body`) probes it on the miss of
the site's own sixteen hashed ways (`emit_mega_dispatch_table_probe`), and the
table is filled from the MIC helper's resolving arms, the lambda-thunk install
and `jit_invoke_dispatch`'s virtual arm. So an `invokeinterface` over 12, 16 or
24 classes whose bodies are compiled now runs MIC-or-PIC, the site's hashed
ways, the shared table, then `CALL` -- no Rust -- for every receiver whose way
was published. What follows is every remaining shape, found by reading, in
which such a call still enters `jit_invoke_virtual_mic` or `jit_invoke_dispatch`
on every execution (or on many).

## 1. A single-pass site inside a `try` in a method with precise exception frames gets no inline cache at all

`jit/src/x64/op_invoke.rs` ~7533: `protected_precise_handler_call =
self.precise_exception_frames && self.pc_is_protected(pc)`, and
`inline_virtual_ic_allowed` requires `!protected_precise_handler_call`. With
it false there is no MIC, no PIC and no hashed stub (the stub is under the same
`inline_virtual_ic_allowed`, ~8269), so EVERY call at such a site is a helper
call, monomorphic or not. `try { visitor.visit(node); } catch (...)` in a
method whose handler reads locals is exactly this shape. The reason given is
that a direct entry cannot publish the caller's precise exception frame
(`emit_post_invoke_exception_check` records it on the dispatch path). The IR
tier has no such gate (`lib.rs` ~33917 plans an IC for every admitted virtual
or interface site).

Proposed: emit the caller-frame publication on the IC hit paths too (the same
record `emit_post_invoke_exception_check` writes, after the `CALL` and before
the sentinel test), or publish it once before the cascade, then drop the gate.
Owner: the single-pass invoke emitter (lane mega2's `op_invoke.rs`), with lane
exc for the frame record. Confirm with `R12MegaIface` rewritten with the calls
inside `try` blocks whose handler reads a local, `CRATONVM_DBG=mic-prof`: the
helper entry count equals the call count today.

## 2. Over-wide sites are blind in the IR tier and stub-less in both tiers

`ir_lower.rs::ir_ic_admits_arity` admits `num_args + 1 <= regs +
IR_IC_MAX_STACK_WORDS` (4); `runtime_lowering.rs::HASHED_STUB_MAX_STACK_WORDS`
is 4 too. A receiver plus eight arguments on Win64 (receiver plus ten on SysV)
gets `jit_invoke_dispatch` on every call in the IR tier and no hashed stub in
either tier. W2-2 (`jit-r12-calls-proposals.md`) proposes raising both caps to
8; every consumer is parametric.

## 3. The shared table is shared only within ONE caller class

The selector interns `(class name, method name, descriptor, invoke kind,
declaring_class_id, owner_class_id)` (`MegaDispatchTable::selector_id`, from
`helpers.rs::install_mega_dispatch_way`), and `declaring_class_id` is the
CALLER's class. Two sites of `Shape.area` in two different classes use two
selectors, so each fills its own copy of every way. The resolution the key
must capture depends on the caller only through its defining loader (which
resolves the constant-pool class name) and through access checks, which fail
before any publication. Proposal M3-2.

## 4. An unbound site reads nothing, and a site binds only by publishing

The machine probe reads the selector from the site's PIC
(`MEGA_DISPATCH_SELECTOR_OFFSET`, 0 = skip). A site binds only inside
`install_mega_dispatch_way`, i.e. the first time ITS OWN helper publishes a
body. A recompiled caller (fresh PIC slots) therefore pays at least one full
resolution per receiver class before its stub reads the table, even when the
table already holds every way it needs. Proposal M3-5 (bind at compile time).

## 5. A full set is never evicted; the table is 1024 ways for the whole VM

`MEGA_DISPATCH_SET_BITS = 8`, `MEGA_DISPATCH_WAYS = 4`: 256 sets of 4 ways per
VM, never evicted (`MegaDispatchTable::install`: "a full set publishes
nothing"). A receiver whose set filled first stays on the helper for the life
of the process (its per-thread `VIRTUAL_DISPATCH_CACHE` entry serves it after
the first call, in Rust). Every supersede scans all 1024 ways under the writer
lock (`retire_matching_locked`), so growing the table as it stands trades
dispatch for tier-up cost. Proposal M3-3.

## 6. Blind IR sites never read the table

`jit_invoke_dispatch`'s virtual arm now PUBLISHES into the table (round 12
wave 3) but reads only its per-thread memo, which it fills after
`jit_invocation_threshold` slow calls per (site, class, thread) through the
generic `invoke_virtual`. A memoized selector per site would let it read the
table first. Proposal M3-4.

## Not a round trip, noted while reading

- A lambda-adapter thunk keeps jumping to the impl body it was built for; a
  later C2 body of the impl does not re-point it (thunks are not in the JIT
  cache maps, and `JitCache::put`'s supersede retires ways naming the IMPL's
  entry, not the thunk's). Correct, but a SAM site keeps a C1 impl for the
  life of the thunk -- per-site PIC ways and the shared table alike.
- Every virtual or interface site with a PIC now carries the ~142-byte probe
  out of line after its stub. Proposal M3-7 shares one copy per VM.

## How to confirm

`C:\craton\jitr12-probes\src\R12MegaIface.java` phases `mega24`, `exc18`,
`sites4` with `CRATONVM_DBG=mic-prof`: `[DISP_CENSUS]` helper entries per phase
against `CRATONVM_JIT_HASHED_STUB_MEGA_TABLE=0`. Each shape above is a variant
of one of its phases (a `try` around the call; a nine-argument interface
method; the four sites split across two classes).

## Round 12 wave 4 (lane mega3)

Landed (see `jit-r12-calls-proposals.md`, "Wave 4 (lane mega3)", for the
design and its soundness table):

- **Shape 3 (selector per caller class): FIXED.** The selector is interned
  under the caller's defining loader (`helpers.rs::mega_selector_context`,
  `CRATONVM_JIT_MEGA_SELECTOR_BY_LOADER`), so every site of a method in one
  namespace shares one selector and every way.
- **Shape 5 (1024 ways, full sets never evicted, O(table) retire): mostly
  fixed.** `invokevirtual` sites, and `invokeinterface` sites with a PIC, now
  have a per-class cell indexed by a column bound on the site
  (`MegaDispatchTable::install_with_class_slot`, the stub's
  `emit_mega_class_slot_probe`), which has no set conflicts and no fixed
  capacity (a 2^18-column cap per VM). Supersede and invalidation retire
  through an owner index in O(ways of the named bodies)
  (`CRATONVM_JIT_MEGA_TABLE_OWNER_INDEX`). Left: a column-less site (a blind
  interface publisher, or a receiver whose cell another selector holds) still
  depends on the 1024 hashed ways, and nothing is reclaimed on class or
  loader unloading (`r12w4-mega3-shared-table-never-reclaims-FIXED-20260928.md`).
- Probes: `R12Mega3VirtualSlots` (virtual 24/48 classes, eight caller
  classes, interface control), `R12Mega3LoaderSlots` (four loader copies).

Still open: shape 1 (protected sites with precise handlers get no IC, M3-6),
shape 2 (over-wide sites), shape 4 (a site binds only on its own first
publication; W4-2 binds at compile time), shape 6 (blind IR sites read nothing;
W4-3). An `invokeinterface` whose implementations declare the method at
different vtable depths misses its cell for the receivers off the first one's
slot (W4-1a).

## Round 12 wave 5 (lane mega4)

Nothing landed against the six shapes; all six stay as the wave-4 section
leaves them. This wave measured, by reading, what a megamorphic call costs once
none of the shapes applies. That is `R12Mega3VirtualSlots`, whose hot loops
never enter the helper. The result is
`r12w5-mega4-megamorphic-call-time-accounting-20260926.md`: the dispatch path
is about a fifth of an iteration, and the rest is the compiled call protocol
and a second, unspliced call. Two more places a receiver can end up on a slower
path than it needs, both ranked in `jit-r12-calls-proposals.md` "Wave 5":

- the per-site hashed ways also hold the four receivers the inline ways
  already answer (`JitPICSlot::install` fills the hashed set first), so a
  PIC-inline site has 12, not 16, hashed ways for overflow receivers (W5-6);
- a site the profile calls megamorphic still runs the PIC cascade and the
  per-site hash before its class cell (W5-4).

Probe: `R12Mega4OneSite` (one site per phase, OSR and method-entry shapes).

## Round 12 wave 7 (lane mega6)

None of the six shapes landed this wave. Found and fixed instead: the class
cells of receivers whose callee tiered up inside a compiled loop were never
refilled (`r12w7-mega6-retired-cells-never-refill-20260927.md`), which hit
the interface shape hardest (`iface24`: every `Q*.g` / `L*.g` body is first
compiled inside an OSR loop). Shape 5's reclamation half moved: an unloaded
class's cell array is now freed once graced
(`r12w4-mega3-shared-table-never-reclaims-FIXED-20260928.md`, wave-7 section).
Read again for shape 1 (protected precise-handler sites): the IC hit paths
already run `emit_inline_callee_deopt_check` and then join `.done`, where the
post-invoke exception check that publishes the precise frame
(`emit_post_invoke_exception_check`) runs for the dispatch path; whether a
cached hit can join the same check needs the exception lane's review of what
a callee sentinel serviced inline leaves pending. Left as M3-6.

## Round 13 wave 2 (lane mega)

None of the six shapes landed; all were re-read against the current code and stand
as the wave-7 section leaves them (shape 3 fixed, shape 5 mostly fixed; 1, 2, 4 and
6 open). What changed at these sites this wave is the recursion stack check
(`r13w1-crash-ic-recursion-stack-guard-patch-FIXED-20260928.md`): every single-pass
IC/stub site now opens with `CMP RSP,[rbp-floor]; JBE` and every IR IC site with
`MOVZX EAX,SP; CMP EAX,imm32; JA` -- two or three instructions on every virtual
call, plus the single-pass prologue's floor fetch in methods with an IC site. Price
it with `CRATONVM_JIT_IC_STACK_GUARD=0` in the probe below before reading any
shape's number against round 12's.

New probe for the shapes: `C:\craton\jitr13-probes\src\R13MegaSiteShapes.java`,
one timed phase per shape over the same 24 classes: `plain` (reference), `tryread`
(shape 1: a try whose handler reads a local), `wide` (receiver + 7) and `wider`
(receiver + 10: shape 2, past both caps on both ABIs), `twoclass` (shape 3, now one
selector). Read each phase against `plain`, with `CRATONVM_DBG=mic-prof` for the
helper entry counts, in `CRATONVM_C2_ACCEPT=never` and `=always`.

Ranked next steps (details in `jit-r13-mega-proposals-RETIRED-20260929.md`): shape 1 needs the
exception lane (a cached hit must publish the precise frame before its sentinel
check) and is the one with real-code reach (`try { visitor.visit(n) } catch`);
shape 2 is a two-constant change (`IR_IC_MAX_STACK_WORDS`,
`HASHED_STUB_MAX_STACK_WORDS`, and the single-pass `IC_MAX_STACK_WORDS`, to 8) whose
only risk is the Win64 outgoing block size, but the probe should first show a
`wider` phase that matters; shapes 4 and 6 are warm-up costs (one helper resolution
per receiver per fresh site), not steady state.

## Round 13 wave 5 (lane callcost2)

None of shapes 1, 2, 4, 6 landed; they stand as the wave-2 section leaves them. Shape 1 is the
single-pass invoke emitter's (`op_invoke.rs`) and the exception lane's; shape 2's single-pass
cap (`IC_MAX_STACK_WORDS`) is not in this lane's files and the wave-2 section's precondition (a
`wider` phase that matters in `R13MegaSiteShapes`) is still unmeasured; shapes 4 and 6 are
warm-up costs (one helper resolution per receiver per fresh or blind site), not steady state.
What did land at these sites is on the accounting page: the optimizing tier's megamorphic edge
now enters the hashed stub's gate entry (M13-2, `CRATONVM_JIT_IR_MEGA_GATE_ENTRY`), which
changes the cost of every IR site's stub path but none of the six shapes' helper counts.
Confirm that with `R13MegaSiteShapes` under `CRATONVM_DBG=mic-prof`: the helper entry counts per
phase must be identical with the switch on and off.

## Round 13 wave 8 (lane mega7)

A seventh shape, found and fixed this wave: **loader churn fills a site's own caches with dead
receivers.** The unload pass retired a site's MIC and PIC ways by target only, so a way keyed on an
unloaded receiver whose target survives (an inherited or default method) stayed live forever: the
four inline ways and sixteen hashed ways filled with classes that can never come back, a fifth dead
class flagged the site megamorphic, and a MIC filled by a dead class served nobody again. Every
receiver of the next generation then went past them to its class cell (an interface receiver off the
first receiver's depth has none, W4-1a), the shared hashed ways, and past a full set to the Rust
helper. Fixed by `r13w8-mega7-site-caches-keep-unloaded-receivers-FIXED-20260928.md`
(`CRATONVM_JIT_IC_FORGET_DEAD_RECEIVERS`, default on).

Shapes 1, 2, 4 and 6 are unchanged (single-pass `op_invoke.rs`, the IR IC admission in `ir_lower.rs`,
and the stub caps; not this lane's files). New probes:

* `C:\craton\jitr13-probes\src\R13Mega7IfaceDepths.java` -- W4-1a measured: sixteen implementations
  declaring `ap` at four vtable depths (direct, under three extra virtuals, under six, inherited from
  a class that does not implement the interface) plus a default-method site, against a flat control
  site of sixteen same-depth receivers. `CRATONVM_DBG=mic-prof` per phase; the itable-indexed cell
  design that would close W4-1a is proposal M7-2 in `jit-r13-mega7-proposals-RETIRED-20260929.md`, to be done only if
  `depths16` shows a gap to `flat16`.
* `C:\craton\jitr13-probes\src\R13Mega7LoaderChurnIface.java` -- the churn shape above (interface and
  virtual sites, 16 receivers per round, 12 from a loader that dies).

## Round 13 wave 10 (lane mega8)

Re-read against HEAD `1e4c0055c`; shapes 1, 2, 4 and 6 stand exactly as the wave-2 section describes
them, and none is in this lane's files:

* shape 1: `jit/src/x64/op_invoke.rs` ~7834 still sets `protected_precise_handler_call` and gates
  `inline_virtual_ic_allowed` on it (lane callcost4's file, plus the exception lane's review);
* shape 2: the three caps are still 4 (`runtime_lowering.rs:146` `HASHED_STUB_MAX_STACK_WORDS`,
  `ir_lower.rs:461` `IR_IC_MAX_STACK_WORDS`, `op_invoke.rs:10726` `IC_MAX_STACK_WORDS`);
* shape 4 (bind the selector at compile time) needs the compile-time planner (`lib.rs` / `x64`);
* shape 6 (a blind IR site reads the table) would intern a selector per call in
  `jit_invoke_dispatch`'s virtual arm unless the site memoizes it; `jit_invoke_dispatch` is the
  call-entry lane's helper, and the cost is warm-up only (one resolution per receiver per thread).

What changed at these sites this wave is the seventh shape's aftermath: the ways an unload frees are
now free for the next loader generation at once (`CRATONVM_JIT_IC_DEAD_WAYS_FREE_AT_ONCE`, see
`r12w7-mega6-grace-starves-while-a-thread-stays-compiled-CLOSED-20260929.md`, wave-10 section), where they
used to wait for a grace a spinning thread withholds; and every inline-cache writer now queues the
owners it withdrew in one batch (`CRATONVM_JIT_IC_BATCH_RETIRED_OWNERS`). Neither changes a helper
count in the shapes above. New probe covering shapes 1, 2 (under the caps) and 3 with in-loop checks
at 8, 12 and 16 receivers: `C:\craton\jitr13-probes\src\R13Mega8MegaShapes.java` (`guarded<N>` is
shape 1 with a handler that runs and reads the loop's locals); read `CRATONVM_DBG=mic-prof` per
phase against `iface<N>`.

## Round 13 wave 11 (lane mega9)

Two of the four open shapes moved; the page stays OPEN for shape 1's default and shapes 4 and 6.

* **Shape 2 (over-wide sites): fixed up to eight stack words** (proposal M13-8, default on,
  `CRATONVM_JIT_IC_WIDE_STACK_WORDS`). The three caps are now one,
  `jit/src/inline_cache_pic.rs::ic_max_stack_words` (8), read by the single-pass MIC block
  (`x64/op_invoke.rs::ic_stack_arg_block`), the optimizing tier's cache admission and hit block
  (`ir_lower.rs::ir_ic_admits_arity` / `ic_wide_stack_block`, the planner asks the first) and the
  hashed stub (`runtime_lowering.rs::hashed_stub_admission`). A receiver plus 8 arguments on Win64
  (plus 10 on SysV) now gets a cache and the stub in both tiers. Left: wider sites (Win64 receiver
  + 9 and up; proposal M9-7, only with a measured site), and the single-pass tier gives an over-wide
  site a MIC and the stub but no inline PIC ways (proposal M9-4).
* **Shape 1 (protected sites with precise handlers): landed opt-in** (M13-7,
  `CRATONVM_JIT_SP_IC_PROTECTED_SITES=1`). The site keeps its caches where the pre-call spill is the
  hoisted one that dominates the hit paths; every hit already joins `.done`, where
  `emit_post_invoke_exception_check` records the same reason-9 frame as the dispatch path, and a
  hit's sentinel is serviced by `jit_service_callee_deopt`, the routine the helper's own door runs
  (`handle_compiled_callee_deopt_sentinel`, "They MUST agree"). Default off until
  `C:\craton\jitr13-probes\src\R13Mega9ProtectedSites.java` (the Spring `invokeListener` shape of
  `a523715a84`: a local assigned before the `try` read in the handler, plus nested, reassigned and
  null-handler variants over mono/poly/mega/interface sites) matches HotSpot with the switch on;
  then proposal M9-2 flips it.
* Shapes 4 and 6 are unchanged. Related warm-up change: a site whose receiver profile already shows
  more than four classes is born megamorphic (M13-9, `CRATONVM_JIT_IC_PROFILE_MEGA_SEED`), so a
  recompiled caller's receivers go cell-first from its first call; it binds no selector (shape 4
  proper is still M3-5).

How to confirm: `R13Mega9WideSites` (receiver + 8/10/12, virtual and interface) with
`CRATONVM_DBG=mic-prof`: on Win64 `mic_calls` stays at warm-up counts for the `v8`/`i8`/`v10`/`i10`
phases in the default arm and climbs with the call count under `CRATONVM_JIT_IC_WIDE_STACK_WORDS=0`;
`R13Mega9ProtectedSites` likewise with `CRATONVM_JIT_SP_IC_PROTECTED_SITES=1` against off, under
`CRATONVM_C2_ACCEPT=never`. Outputs identical in every arm.

## Round 14 wave 1 (lane calls)

Re-read against `adb9178bc`; nothing landed against the shapes this wave (the lane's code change is
CC5-1, the call-crossing residency of the `fib` page). Where each stands:

* **Shape 1** (protected sites with precise handlers): still opt-in
  (`CRATONVM_JIT_SP_IC_PROTECTED_SITES`, `x64/op_invoke.rs`). Its flip is proposal M9-2, gated on
  `R13Mega9ProtectedSites` matching HotSpot with the switch on and a Spring census; neither run is
  recorded in `C:\craton\jitr13-probes\ORCH-LOG.md` or the round-14 log. That run, not code, is next.
* **Shape 2** (over-wide sites): fixed to eight stack words in round 13 wave 11; wider needs a
  measured site (M9-7).
* **Shapes 4 and 6** (a site binds its selector only by publishing; a blind IR site reads nothing):
  warm-up costs only (one helper resolution per receiver per fresh or blind site and thread), and
  M13-9's profile seed already sends a recompiled caller's receivers cell-first from its first call.
  Neither is worth its planner / helper change before M9-1's counters say the steady state is
  understood (`m91-mega-anatomy.sh`, see the accounting page's round-14 section).

Status stays OPEN (shape 1's default, shapes 4 and 6).

## Round 14 wave 2 (lane mic)

Re-read for anything cheap inside this lane's files (`JitMICSlot` / `JitThreadQuiescence` / MIC regions
of `jit/src/lib.rs`, `jit/src/inline_cache_pic.rs`, the MIC miss-handler and grace regions of
`vm/src/jit/helpers.rs`); none of the open shapes lives there:

* shape 1's flip is proposal M9-2, gated on a run (`R13Mega9ProtectedSites` against HotSpot with
  `CRATONVM_JIT_SP_IC_PROTECTED_SITES=1`), not on code;
* shape 4 needs the compile-time planner to bind the selector (M3-5);
* shape 6 is `jit_invoke_dispatch_body`'s virtual arm, outside the MIC regions, and warm-up only.

What did change at these sites: a receiver refused a way only because a retired way's grace lagged
(a live receiver's way retired by a tier-up while some thread stayed in compiled code) now gets that
way as soon as every running thread has entered a miss handler since the retirement (M8-1,
`CRATONVM_JIT_IC_QUIESCENT_STAMP`, see `r12w7-mega6-grace-starves-while-a-thread-stays-compiled`),
instead of taking the helper until a depth-0 return or a stop. That removes helper round trips of a
seventh, transient kind (the `mic_grace_lag_refused` count) and none of the six shapes. Status
stays OPEN (shape 1's default, shapes 4 and 6).

## Round 14 wave 3 (lane calls)

Re-read against `20a1dbb4f`; none of the open shapes moved, and none is in the calls lane's files
this wave:

* **Shape 1** (protected sites with precise handlers): `x64/op_invoke.rs` still gates the
  single-pass cascade on `CRATONVM_JIT_SP_IC_PROTECTED_SITES` (opt-in). The flip is proposal M9-2,
  gated on `R13Mega9ProtectedSites` matching HotSpot with the switch on (`CRATONVM_C2_ACCEPT=never`)
  plus one Spring census; `C:\craton\jitr14-probes\ORCH-LOG.md` records neither. That run is the
  whole remaining work for shape 1.
* **Shapes 4 and 6** (a site binds its selector only by publishing; a blind IR site reads nothing):
  warm-up only. Binding at compile time needs the VM's selector interning
  (`helpers.rs` `mega_selector_context`) reachable from the planner, i.e. a new compile-time
  callback; not worth it before M9-1's counters (`m91-mega-anatomy.sh`) show warm-up matters.

Related change this wave, not one of the shapes: the optimizing tier's merged direct cross calls
now leave for a deferred cold tail on a not-taken `JO` (CC3-1,
`CRATONVM_JIT_IR_CROSS_CALL_COLD_TAILS`), and the wave's census line (`CRATONVM_DBG_JITC=1`,
`ir-call-cold-tails ... ic_hit_exits=<n> ic_admissible=<m>`) says how many inline-cache hit exits
the same deferral could take. Status stays OPEN (shape 1's default, shapes 4 and 6).

## Round 14 wave 7 (lane mega)

Re-read against `59a41a0d8`. Shape 1 still waits for its run (M9-2: `R13Mega9ProtectedSites` with
`CRATONVM_JIT_SP_IC_PROTECTED_SITES=1` against HotSpot, `CRATONVM_C2_ACCEPT=never`, plus one Spring
census; `C:\craton\jitr14-probes` records the probe only in its default arms, `oi-base-bat-*.txt`
and `res-*`, never with the switch on). Shapes 4 and 6 had no measurement behind "warm-up only";
this wave adds the one for shape 4.

* **Shape 4, corrected reading.** A site does not pay "one full resolution per receiver class"
  before its stub reads the table: it pays full resolutions only until its OWN first publication
  binds the selector (`helpers.rs` `install_mega_dispatch_way` -> `JitPICSlot::bind_mega_dispatch`),
  after which every receiver the table holds is answered in machine code by the hashed stub's table
  probe, or in Rust by `try_mega_dispatch_table_entry`. So the cost is the receivers a fresh site
  resolves before its first compiled callee publishes: small when the first receiver's body is
  already compiled, larger when the site's first receivers are interpreted callees (nothing
  publishes, nothing binds).
* **Landed: the census that prices it** (`CRATONVM_DBG=mic-prof`, `[DISP_CENSUS]`
  `mic_unbound_table_held`; `vm/src/jit/helpers.rs` `note_unbound_selector_table_held`, called just
  before `try_mega_dispatch_table_entry` in `jit_invoke_virtual_mic_body`'s megamorphic arm). It
  counts calls at a site with NO bound selector for which the VM's table already held a way under
  the selector the site would bind -- the full resolutions a compile-time bind (M3-5) would have
  turned into table hits. It peeks (`MegaDispatchTable::interned_selector_in_context`, new, never
  interns; test `r14w7_mega_interned_selector_tests`) and binds nothing, so the arm it measures runs
  unchanged; off without mic-prof. Probe: `C:\craton\jitr14-probes\src\R14MegaUnboundSites.java`
  (a warmed 24-receiver virtual site, then fresh sites of the same method in the same class and in
  another class of the same loader, and a fresh interface site as the control).
  Decision rule: `mic_unbound_table_held` in the tens per fresh site -> shape 4 is warm-up, close it;
  growing with the call count -> land proposal MG7-1 (`jit-r14-mega-proposals.md`: bind at the
  site's first successful resolution instead of its first publication).
* Shape 6 (a blind `jit_invoke_dispatch` site reads nothing) is `jit_invoke_dispatch_body`'s
  virtual arm, the interpreter round's hunk; the same census there is proposal MG7-3.

Status stays OPEN (shape 1's run; shapes 4 and 6 until their counts are read).
