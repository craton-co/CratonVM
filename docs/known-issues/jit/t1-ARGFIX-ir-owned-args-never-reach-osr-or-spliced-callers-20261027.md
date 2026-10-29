> **Row filed 2026-10-29 (gcj j2/pn2 retirement, branch `claude/by-name-dispatch-pins-door-register-90d57b`): `tools/bench/Gcd1ArgPinSpecialProbe.java more settle` is this page's.** The single-pass tier now matches HotSpot on all six shapes (`PASS all 6` under `CRATONVM_JIT_IR_CALL=0 CRATONVM_JIT_IR_CALL_SPECIAL=0 CRATONVM_JIT_IR_CALL_VIRTUAL=0 CRATONVM_C2_ACCEPT=never`, Generational / G1 / ZGC, 20/20; see `docs/internal/gc/gcj-j2pn2-by-name-dispatch-pins-and-door-register-image-keep-dropped-arguments-FIXED-20261027.md`). What remains is the optimizing tier's: the default arm prints `FAIL 6 of 6` on every collector at `-Xmx64m` (HotSpot `PASS all 6`), unchanged with `CRATONVM_JIT_BY_NAME_TAIL_UNPINS_ARGS=1 CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE=1` and with `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS=1 CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY=1`; with IR call lowering off but the optimizing tier still compiling, `spliced-direct` alone fails, holder `splicedCaller off=112 region=operand-spill tier=ir in_map=true` (the argfix2 window at a spliced call). Use it as the retention row for items 1-3: it should print `PASS all 6` in the default arm once the optimizing tier's calls give their arguments to the callee.

> **j27 wave 9 (lane W9RPL, 2026-10-29): item 3 (OSR half) NOT FIXED. The OSR service-copy term is
> wired (opt-in) and reaches the OSR site, but it clears nothing.**
> - **Wiring.** In `ir_lower::Lowerer::ir_direct_call_site_admits_service_copy_window`, the
>   `osr_compile` argument is now
>   `crate::osr_service_copy_window::service_copy_window_osr_term(self.graph.osr_compile)`. The owned
>   hand-off (`ir_direct_call_args_owned_by_callee`) keeps the plain term. Switch:
>   `CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW` (already registered, opt-in). Unit tests
>   `osr_service_copy_window::tests::*` (2) pass.
> - **Engagement.** New probe `tools/probes/jit/j27/J27OsrServiceCopyRows.java`, `-Xmx96m`,
>   `CRATONVM_JIT_IR_INLINE=0`. With the switch on, the OSR compile prints `window n33 bci=20: none,
>   the call is on a cycle`. With it off, the OSR site never asks.
>   - `argument_only_homes` refuses every call on a loop cycle, in method-entry compiles too.
>   - The after-loop site (bci 41) is refused by a site term in both kinds of compile.
>
>   So both rows OOME with and without the switch (HotSpot 25: `false`). Default splicing passes both
>   rows. New page: `j27-service-copy-window-names-no-loop-or-after-loop-osr-call-20261029.md`.
> - **No regression with the switch on.** These probes match HotSpot in `--jdk-only` and
>   `--compatible`, with the switch on and off:
>   - `J27W8ArgDeclineRows` (plus `CRATONVM_DBG_FORCE_CALLEE_DECLINE=J27W8ArgDeclineRows.leaf`);
>   - `T1W12ArgfixOwnedTempArg -Xmx96m` (`temp-arg big ok len=54525952 chain=106496`);
>   - `J27ArgfixSpliceRows -Dj27.big=true`.
>
>   `R21OomArgWindow` facts are SAME, and `watched-oome true` is unchanged.

> **j27 wave 8 (lane W8ARG, 2026-10-28): REVIEWED; item 3 still NOT FIXED (OSR half), PENDING HOST

> **j27 triage (builder lane TRIB, 2026-10-28, Windows, release LTO-off build of the j27 tree at 20:34 = `C:\craton\j27-trib-bin\cvm-trib-1.exe`): item 3 VERIFIED: nothing to wire.** `j27_t1splice_` 7 tests pass. `CRATONVM_DBG_IR_SLOTS=1` on `J27ArgfixSpliceRows -Dj27.big=true`, `T1W12ArgfixOwnedTempArg -Xmx96m` and `J27OwnedArgsPrefixRows` (facts equal HotSpot in both runs, and in `-Dj27.big=true` with `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY=1`, which prints HotSpot's `fact big ok len=13631488 b=13631488`). Every `level-replay=Ok([1-9]` line with `admitted=false` is a JDK site, `ArraysSupport.hashCodeOfUnsigned` (String hashing, 16 per probe). The probes' retained-arg sites (`outer`, `outerBig`, `tempArg`, `oome`) answer `Err(Osr)` / `Err(Commit(1))`. Under `..._SPLICE_REPLAY=1` the `leaf` site is `admitted=true owned=true splice-replay=Ok(..)`. The recipe's condition for building the wiring is not met.
> VERIFICATION.** Splices: the two-level rule and its window half were re-read and stand (status
> on the two-level proposal page). This wave adds the decline lever `CRATONVM_DBG_FORCE_CALLEE_DECLINE`
> and the probe `tools/probes/jit/j27/J27W8ArgDeclineRows.java`, the first run of the hand-off's
> re-run. The wave-6 level-replay census (`level_replay_plan`) is correct as a census. Two
> constraints the owed wiring must honour, recorded here because nothing checks them yet:
> - **Level `k`'s parameters must not be dropped by the keep set.** `prefix_keeps_param_slots`
>   proves that the prefix writes no parameter slot. The chain state at the call can still have
>   NULLED a parameter that level `k`'s liveness calls dead at the parked bci
>   (`splice_snapshot_dead_slots`, default ON). Wiring step 1 must record the bci-0 snapshot with
>   every parameter slot of level `k` named, or the rebuilt frame reads `null`. Its liveness at
>   bci 0 already keeps every parameter the level reads later.
> - **Forwarder levels.** `bodies_enclosing` skips a lambda SAM forwarder, so `k` never names one.
>   A re-run from the body under a forwarder returns straight to the forwarder's caller. That is
>   sound only for the forwarder shape the builder admits (nothing after the call but the
>   `return`). Wiring step 4 must refuse an opaque forwarder, as `splice_scope_chain` does.

> **j27 wave 6 (lane T1SPLICE, 2026-10-28): item 3 still NOT FIXED. The decision half of a third route
> is built and printed as a census only; nothing is wired, so behaviour does not change and there is no
> switch. PENDING HOST VERIFICATION (unit tests and the census).**
>
> **The route: level replay.** The two-level rule re-runs the PHYSICAL frame. A decline could instead
> resume the interpreter like this:
> - levels `0..k` are rebuilt from the owned call's chain state, each parked at the invoke it already
>   stands at (nothing ran in those frames since);
> - spliced level `k >= 1` is rebuilt at bci 0 with only its parameter slots, read from its own scope
>   of that state;
> - levels `k..` are then re-run.
>
> That is exact when three things hold:
> - every re-run level passes the single-level terms (`deopt::owned_args_caller_replay_is_exact`
>   unchanged and still the one exactness rule, plus no handler row at the parked pc);
> - level `k`'s loop- and handler-closed prefix writes none of its parameter slots;
> - no lock exists in the compile.
>
> The outer frame is never re-run, so the route serves OSR bodies (at a spliced site) and outer prefixes
> that commit. A non-spliced site in an OSR body still has no route (`level-replay=Err(Osr)`).
>
> **Code.** New in `jit/src/owned_args_splice_replay.rs`:
> - `level_replay_plan` returns `Ok(0)` exactly when the two-level rule admits; otherwise the smallest
>   `k >= 1`, or the bounding refusal (new variant `ChainRefusal::ParamWritten`);
> - `spliced_call_level_replay`, the lowerer's entry point;
> - `prefix_keeps_param_slots` / `written_local_slot`;
> - `ReplayLevel` gains `num_params`.
>
> `jit/src/ir_lower.rs` gains `Lowerer::owned_args_level_replay`. `ir_direct_call_args_owned_by_callee`
> appends ` level-replay=Ok(k)|Err(..)` to the `CRATONVM_DBG_IR_SLOTS` `[ir-owned-args] site` line of
> every spliced or OSR site, computed only under that flag. `ir_direct_call_site_admits_owned_args`, its
> `!osr_compile` term and `owned_args_caller_replay_is_exact`'s signature are untouched, so dev's gkr
> window and hotfix 8335e345ff are unaffected.
>
> **Tests:** `cargo test -p cratonvm-jit --lib j27_t1splice_` (7 tests). They pin OSR `Ok(1)` where the
> physical rule says `Osr`, outer commit to `Ok(1)`, plain / `wide` / `iinc` parameter writes, the
> three-level choice, and the loop-closed extent.
>
> **Census first; it decides whether the wiring is worth building.** Run `CRATONVM_DBG_IR_SLOTS=1` on
> `J27ArgfixSpliceRows -Dj27.big=true`, `T1W12ArgfixOwnedTempArg -Xmx96m` and `J27OwnedArgsPrefixRows`,
> then `grep '\[ir-owned-args\] site.*level-replay=Ok([1-9]'`. Build the wiring only if that finds an
> `admitted=false` site whose argument is the retained object.
>
> **Wiring still owed, in this order (each opt-in):**
> 1. The builder records, per owned spliced site, the snapshot "levels `0..k` parked, level `k` at pc 0
>    with locals = its parameter slots".
> 2. The owned site's cold side takes that snapshot as a deopt exit when the service reports a
>    hand-off, instead of leaving frameless (`FramelessTrapSite::Handoff`). The VM service must return
>    to the compiled caller for this.
> 3. `snapshots_readable_after_call` counts that snapshot as readable, so level `k`'s parameters stay
>    rooted and only arguments computed in levels `k..` are windowed.
> 4. The chain deopt must accept a chain whose innermost scope (level `k`; the deeper levels are
>    dropped) resumes at bci 0 with an empty stack, while its caller scopes resume after their invoke.
> 5. dev's service-copy window (`rerun_window_closed_at`) must count such a site as a re-run point of
>    level `k`, not of the physical frame.

> **j27 wave 5 (lane OOM21, 2026-10-28): item 3's OSR half is NOT FIXED (design recorded); PENDING
> HOST VERIFICATION of wave 4.** Re-read against the code: an owned site in an OSR body must keep
> refusing. Its decline re-runs the physical frame from bci 0, and an OSR frame has none (parked entry
> locals, `osr_refuse_replay`). The one route that looks sound is dev's gkr service-copy window
> (`CRATONVM_JIT_SERVICE_COPY_WINDOW`). It hands nothing off, and its rule is about the callee's code.
> That route is written up as `j27-osr-direct-call-service-copy-window-proposal-20261028.md`, with the
> two reader questions a build must answer first: the OSR call's sentinel resume at the invoke, and its
> exception exits. It is not built, because the window is not in this worktree and is not checkable
> here.
>
> Related work this wave, on OOM21-2's page:
> - the opt-in rule for constant `putstatic` prefixes (`CRATONVM_JIT_OWNED_ARGS_CONST_STATIC_STORES`);
> - a new defect: dev's window ignores owned-args re-run points
>   (`j27-service-copy-window-ignores-owned-args-rerun-points-FIXED-20261029.md`). That defect also bears on
>   ARGFIX2's spliced hand-off: a spliced owned site is a re-run point of the OUTER frame, and the
>   window must count it too once `..._SPLICE_REPLAY=1` is used with the window on.

> **j27 wave 4 (lane ARGFIX2, 2026-10-28): item 3 PARTLY (splices only, opt-in), PENDING HOST
> VERIFICATION.** The two-level replay rule is built behind `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY=1`
> (status and recipe on `j27-argfix-spliced-owned-args-two-level-replay-proposal-20261028.md`); OSR
> sites stay refused; the window still zeroes nothing at a spliced call
> (`j27-argfix2-owned-arg-window-refuses-every-spliced-call-FIXED-20261029.md`), so no retention changes yet.

> **2026-10-29 exact-source host follow-up: STILL OPEN.** The 15 `j27_argfix2_` unit tests pass and
> the release build succeeds. `J27ArgfixSpliceRows -Xmx96m -Dj27.big=true` matches HotSpot on the
> warm, throw, deopt, after-deopt, and GC facts, and the splice-replay site engages. The retention
> row still prints `fact big OOME Java heap space` with all three owned-argument switches enabled.
> `CRATONVM_DBG_ROOT_KIND_CENSUS=1` attributes the 54,525,968-byte array to the running thread's
> root snapshot (`snapshot "main"`, one object, six snapshot words); the window logged a home clear,
> so this is not yet evidence that the spliced handoff alone owns the remaining root. The profile
> and logs are `/tmp/t1-argfix-root-census-20261029.out` and
> `/tmp/t1-argfix-jdk-enabled-big-phi-20261029.out` on host 64.177. The phi-home candidate change
> did not close the retention gate; no retirement claim is made.

> **j27 wave 2 (lane GCMAP, 2026-10-28): item 3 NOT FIXED (design recorded), PENDING HOST VERIFICATION
> of wave 1's item 2.** Reviewed `ir_direct_call_args_owned_by_callee` / `ir_direct_call_site_admits_owned_args`
> (`jit/src/ir_lower.rs`) and `deopt::owned_args_caller_replay_is_exact`: the decline path re-runs the
> PHYSICAL frame from bci 0, so the two refusals are load-bearing and were left in place. OSR: there is
> no bci-0 state to re-run (the interpreter frame's reference locals are parked at entry,
> `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS`, and `osr_refuse_replay` turns any replay into an
> `InternalError`), so an OSR site needs OOM21-2's weak caller roots or an in-place deopt at the invoke;
> nothing smaller is sound. Splices (non-OSR body): a re-run from the OUTER method's bci 0 is exact when
> every level of the chain commits nothing before the point it is parked at -- the outer method up to
> the splice's `invoke` (`resolve_spliced_pc`) AND each spliced body up to its own inner call, asked with
> that body's code from `spliced_body_codes` -- a two-level form of the existing rule, written up as
> `j27-argfix-spliced-owned-args-two-level-replay-proposal-20261028.md`. Not implemented: unbuildable
> here, both switches are opt-in, and the T1W9 row is not argument retention (CORRECTION below), so no
> row would show the gain. Host still owes wave 1's item-2 checks (unchanged recipe below).

> **j27 wave 1 (lane FRAMES, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** Item 2 reviewed and
> FLIPPED default ON (`CRATONVM_JIT_IR_KEEP_SET_SPLICE_LOCAL_LIVENESS`, `=0` kills;
> `ir_lower::ir_keep_set_splice_local_liveness_enabled` now `runtime_flag_default_on`, its doc carries
> the review). Soundness checked from code: the liveness reads the bytes a deopt resumes (`jit_bridge`
> records `callee_original_code` for every masked or stripped body; `append_ir_inline_site` copies the
> body verbatim, so nested invokes and returns are real instructions to the liveness, not jumps); handler
> rows are the admitted ones (a body with a table is admitted, a stripped fence `finally`, or refused --
> the stripped fence's handler reads only what the normal-path duplicate already reads, and a missed
> read there could only null a `reachabilityFence` argument); a scope pc that is no instruction start is
> not `covered` (stays live); `max_locals` / byte disagreement is not answered; a truncated fixed point
> widens to all-live; a debugger withholds the whole keep set. Item 3 unchanged (OPEN, same design gap as
> OOM21-2). Host must confirm: `cargo test -p cratonvm-jit --lib t1_argfix_` (the switch test now
> asserts default ON), the jit stress battery `verify`/`all` arms and the chain-deopt probes SAME vs
> `=0`, and `T1W9ArgrefOwnedArgs` / `T1W12ArgfixOwnedTempArg -Xmx96m` on ZGC/G1/Gen no worse than `=0`.
> If any of those diverge, revert the one-line default (keep the switch, default off).

# The IR owned-arguments switches never reach the probe's caller (and miss OSR and spliced callers)

> **gkr status (2026-10-28): the T1W9 `temp-arg` row now passes (G1, ZGC, Gen; host 64.177.43.150, `cvm-m1`,
> dev `9194ad22f7` + gkr). Item 3 is still OPEN.** The row was argument retention after all, but not in the place
> item 4 named. Four holders were removed in turn: the single-pass `tempArg` blind spill image's RAX slot, the
> callee-deopt service copy (now cleared once the callee passes its last exact re-run point, the "rerun window"),
> the IR callee's lazy parameter prologue word, and the G1 fragmentation. See
> `docs/internal/fixed-bugs/big-allocation-after-jit-churn-oome-FIXED-20261028.md`. Item 3 (owned hand-off at OSR and
> spliced call sites) still needs a decline path that does not re-run the physical frame.

> **t1 status (2026-10-27, orchestrator, splice-planner pass): item 1 VERIFIED, item 2 IMPLEMENTED
> opt-in (pending the host unit run and the A/B), item 3 OPEN.**
>
> | item | state | evidence |
> |---|---|---|
> | 1. do the switches work where admitted | VERIFIED: `T1W12ArgfixOwnedTempArg -Xmx96m` prints `temp-arg big ok len=54525952 chain=106496` with `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS=1 CRATONVM_JIT_IR_CALLEE_ARG_WINDOW=1` on ZGC, G1 and Gen, both on cvm-fin2 and cvm-dev3 (wave-12 host run) | the switches are not the gap; the T1W9 row's holder is elsewhere |
> | 2. spliced bodies keep every local | IMPLEMENTED, opt-in `CRATONVM_JIT_IR_KEEP_SET_SPLICE_LOCAL_LIVENESS` (`=1`): under the precise keep set, every scope of a chain snapshot drops the locals its OWN liveness calls dead (scope 0 at its instruction, each caller scope at the `invoke` it is parked in; the compiling method's scope by its own liveness). `lib.rs` `spliced_body_codes` hands `ir_lower` each spliced method's unmasked body (handler rows rebased; a masked body with no unmasked copy, or a key whose sites disagree, is left out); `ir_lower` `splice_snapshot_dead_slots` decides per slot. NOT done: the window's `named` closure (`owned_call_arg_window`), which only the two opt-in owned-args switches read | tests `ir_lower` `t1_argfix_splice_local_liveness_tests`, `lib.rs` `t1_argfix_spliced_body_codes_tests` |
> | 3. owned hand-off at OSR / spliced call sites | OPEN (needs a decline path that does not re-run the physical frame) | unchanged |
>
> **Which tier holds the T1W9 argument (host, 2026-10-27, cvm-fin2, `-Xmx96m`, `--jdk-only`).**
>
> | VM / arm | ZGC | G1 | Gen / Serial |
> |---|---|---|---|
> | HotSpot 25 default | big ok | big ok | big ok |
> | HotSpot 25 `-XX:TieredStopAtLevel=1` (C1 only) | big ok | big ok | big ok |
> | HotSpot 25 `-Xint` | **OOME** | **OOME** | **OOME** |
> | CratonVM default | OOME | OOME | OOME |
> | CratonVM `CRATONVM_JIT_IR_INLINE=0` (no splices) | OOME | OOME | OOME |
> | CratonVM `CRATONVM_C2_ACCEPT=never` (single-pass only) | OOME | OOME | OOME |
> | CratonVM `--nojit` | OOME | OOME | OOME |
> | CratonVM owned-args + window, no splices | OOME | OOME | OOME |
>
> So `--nojit` matches HotSpot's interpreter (which also roots the dead argument), and the divergence
> is the SINGLE-PASS tier: HotSpot's C1 alone does not keep the 52 MiB argument, CratonVM's
> single-pass bodies do (no splice involved: it OOMEs with splicing off). The holder is the
> single-pass caller's outgoing argument (`tempArg`) and/or `consume`'s single-pass / OSR frame
> keeping its dead local `a` -- item 4 below; items 2-3 do not reach it.
>
> **CORRECTION (2026-10-27, later the same day): item 4's premise is WRONG; the T1W9 row is not argument
> retention.** The root-kind census at the failing allocation (G1, default and `C2_ACCEPT=never`) reaches
> ~0.84 MB from every root together with ~5 MB of the heap in use; the row passes at `-Xmx112m`. The minimal
> reproducer `tools/probes/t1/T1W13BigAfterChurn.java` (the same churn, then ONE 52 MiB allocation with nothing
> live) fails on CratonVM G1 with the JIT (`big-alone OOME`; the OOME ladder ends `fragmented-slide
> requested=53 free=91 longest=44`: 3 Eden + 1-2 Survivor + 1 Old regions split the Free pool even after the full
> cycle and the slide, which packs Old only) and on ZGC with the JIT after the array is dropped
> (`big-then-chain OOME`; passes with `CRATONVM_DBG_NO_JIT_ROOT_SCAN=1`, `CRATONVM_JIT_IR_INLINE=0` or
> `CRATONVM_JIT_OSR=0`: a stale word in a spliced OSR frame keeps the dead array). HotSpot 25 prints every row
> `ok` on Serial, G1 and ZGC; CratonVM Gen and every `--nojit` run pass. Filed as its own page:
> `docs/internal/fixed-bugs/big-allocation-after-jit-churn-oome-FIXED-20261028.md`.
>
> 4. **(superseded by the correction above) Single-pass oop maps are not liveness-exact for a dead argument.** Narrow the single-pass
>    body's GC map at calls and polls by bytecode liveness (the IR keep set's rule), and drop an
>    outgoing argument from the caller's map once the callee owns it. Next step: `CRATONVM_DBG_IR_SLOTS`
>    has no single-pass twin, so first find which frame holds it (`CRATONVM_DBG_ROOT_SOURCE=1` at the
>    OOME, or a heap-root census), then fix that tier.
>
> Soundness of item 2: same argument as the own-instruction narrowing. A snapshot use extends its
> node's live range, so a dropped local's home holds its value or a cleared null, never another
> occupant, and a resume hands the interpreter that value for a local it never reads. A home the
> lowering cannot locate is still `Unsupported` (refused / keep-everything re-lower), never guessed.
> Host A/B owed: `T1W9ArgrefOwnedArgs -Xmx96m` on ZGC/G1 with and without the switch (temp-arg row),
> plus the which-tier arms in this page's evidence block (IR_INLINE=0, C2_ACCEPT=never, --nojit).

Status: OPEN (t1 wave 12, lane ARGFIX, 2026-10-27). Diagnosed by reading; no code change to the switches.
Area: JIT optimizing tier, I-8 (`jr21-compiled-caller-roots-its-outgoing-arguments-for-the-whole-callee-20261014.md`).

## Symptom

`T1W9ArgrefOwnedArgs -Xmx96m` with `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS=1 CRATONVM_JIT_IR_CALLEE_ARG_WINDOW=1`
(ZGC and G1, `--jdk-only` and `--compatible`, plus the stress arm) prints `temp-arg OOME Java heap space`;
HotSpot 25 prints `temp-arg big ok len=54525952 chain=106496` (host w11 run, `recmp11.txt`).

## Root cause

The row never exercises the two switches, so its ON arm is the OFF arm:

1. **The caller is not an optimizing method-entry body.** `tempArg` is entered 3 000 + 1 times. The C1->C2 bar
   is `c2_threshold = 20_000` (`jit/src/tiered.rs:497`) and the Tier4 call predicate needs
   `i + b >= 15_000` (`TIER4_I_PLUS_B_COMPILE_THRESHOLD`, `jit/src/tiered.rs:2586`); `tempArg` has no back edge,
   so it stays a single-pass body (`c1_threshold = 500`). The IR switches act only in `ir_lower.rs`.
2. **Where an optimizing body does call `consume`, the site is refused by design.** `main`'s loops OSR into an
   optimizing body; `tempArg` (11 bytecode bytes, far under `MAX_INLINE_BYTECODE_SIZE = 325`,
   `jit/src/lib.rs:8081`) is a splice candidate there. `ir_direct_call_args_owned_by_callee`
   (`jit/src/ir_lower.rs`, section above `ir_callee_owned_args_enabled`) refuses a site in a splice
   (`in_splice`) and in any OSR compile (`self.graph.osr_compile`): the protocol's decline path re-runs the
   physical frame from entry, which an OSR body or a splice's enclosing method cannot do.

So the retention the row sees is I-8 in the shapes the switches do not cover: the single-pass caller and/or the
OSR body's splice frame states. Note also that the window's local narrowing (`owned_call_arg_window`,
`own_instruction`) and the precise keep set narrow locals by bytecode liveness only at the compiling method's
OWN instructions; a spliced body's snapshots keep every local of the splice (so a spliced `consume` would keep
`a` named across its whole loop). That is the most likely holder if `consume` itself is spliced.

## HotSpot

C1/C2 call-site oop maps leave outgoing arguments to the callee, whose map is liveness-exact; an inlined callee's
locals are pruned by the callee's own `MethodLiveness`, and the caller's JVMS at an inlined invoke has the
arguments popped. No OOME in any of these shapes.

## Suggested fix

1. Probe: `tools/probes/t1/T1W12ArgfixOwnedTempArg.java` (new) is the row that reaches the switches
   (41 000 entries + a 300 ms install pause, both methods padded over the 325-byte splice limit). Run it first:
   it says whether the switches work where they are admitted.
2. Splices: narrow a spliced body's snapshot LOCALS by the spliced method's own bytecode liveness
   (`regalloc::live_locals_per_pc_all` over the callee's code, keyed by the splice range), in both the precise
   keep set and `owned_call_arg_window`'s `named` closure. Needs the splice ranges' method identity; touches the
   keep set (opt-in switch first).
3. OSR / spliced call sites: the owned hand-off needs a decline path that does not re-run the physical frame
   (OOM21-2's weak caller argument roots, `jit-r21-oom-proposals.md`), or a deopt that rebuilds the interpreter
   frame at the invoke with the arguments re-materialised from the callee's own frame.

## How to verify

```
java -Xmx96m -XX:-UseCompressedOops T1W12ArgfixOwnedTempArg        # temp-arg warm ok / temp-arg big ok len=54525952 chain=106496 / pad 0
CRATONVM_JIT_IR_CALLEE_OWNED_ARGS=1 CRATONVM_JIT_IR_CALLEE_ARG_WINDOW=1 cratonvm -Xmx96m T1W12ArgfixOwnedTempArg
  x {--jdk-only,--compatible} x {default ZGC, -XX:+UseG1GC, -XX:+UseGenerationalGC}
CRATONVM_DBG_IR_SLOTS=1 (same ON arm) 2>&1 | grep '\[ir-owned-args\]'
  expect: site bci=7 T1W12ArgfixOwnedTempArg.consume([BI)I kind=3 admitted=true owned=true
          window n<id> bci=7 ...: homes [<offset>]
```

If the ON arm matches HotSpot with both engagement lines, the switches work and this page narrows to items 2-3
(the T1W9 row stays OOME until then). If it OOMEs WITH both lines, the holder is downstream of the window (the
IR `consume` frame: check `CRATONVM_DBG_IR_SLOTS=1` for `[ir-param-home]`/dead-home clears in `consume`, and the
OOME root census) -- file that as a new defect.

> **2026-10-29 final exact-tree census correction (OPEN).** The exact final rerun still OOMEs with
> all three owned-argument switches and splice replay enabled. The 54,525,968-byte object appears
> under root-census kind #9, `Root snapshot (for cross-thread GC scanning)`, with six snapshot words
> and no provenance; the `main` category itself has zero direct objects in this run. Do not treat
> this as attribution to the named main snapshot or as proof the splice handoff is the sole owner.
