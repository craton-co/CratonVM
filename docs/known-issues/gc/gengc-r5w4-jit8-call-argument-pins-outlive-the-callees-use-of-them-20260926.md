# A compiled callee's reference arguments stay rooted for its whole run, used or not

> **STATUS (2026-09-29, gce e1/x): KEEP -- unchanged, by design.** `*_argpin_sp_1..2` (`verify-e1/ve1`) print `PASS arg-pin-cold-caller`, `FAIL arg-pin-warm-caller`, `FAIL 1 of 2` on all three collectors, on e1 and base alike. **Remaining:** the IR-tier warm caller (row 5 of `gcd-d9d-...`; the durable fix is `gcd-d9d-proposal-no-call-site-rerun-from-entry-20260928.md`).

> **STATUS (2026-09-29, gce e1/f): OPEN, unchanged in code -- by design for
> the probe's warm case.** Re-read against this wave's changes: the warm
> `caller` is the optimizing tier's direct call (row 5 of
> `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes-20260928.md`),
> and `caller` invokes `make` before the call, so any callee-owned-arguments
> rule, in either tier, must keep the copy there (d10/f's replay
> constraint). None of this wave's band claims reaches it: the argument
> VALUES' homes are inputs of the IR call node and named in its map. The
> route that would free it is `gcd-d9d-proposal-no-call-site-rerun-from-entry-20260928.md`
> (no caller re-run at all, so no replay constraint), which is a design item,
> not a fix for this lane. **Run:** none new; `Gcd1ArgPinProbe` is expected to
> keep printing `PASS arg-pin-cold-caller`, `FAIL arg-pin-warm-caller`,
> `FAIL 1 of 2` on every collector.

> **STATUS (2026-09-28, gcd d10/f second commit, lane frames10): OPEN --
> the probe's warm site is, by the replay rule, NOT a site whose arguments a
> caller may leave to the callee; d9/d's fix does not reach it and must not.**
>
> - **Measured (orchestrator, round branch `bd19227a3`, d9/d in):** default
>   `PASS arg-pin-cold-caller`, `FAIL arg-pin-warm-caller`, `FAIL 1 of 2`, 3/3
>   on Generational (identical to the d84029110 baseline); Generational with
>   `CRATONVM_DBG=oldmark-root-census,root-source CRATONVM_DBG_IR_SLOTS=1`
>   `PASS all 2`; G1 and ZGC with that env `FAIL arg-pin-warm-caller`.
> - **Which shape the warm call is, by reading:** the OPTIMIZING tier's
>   direct call (row 5 of
>   `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes-20260928.md`),
>   not d9/d's single-pass one. The IR tier is NOT off under moving-young:
>   `docs/internal/retired/jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md`
>   (`f78b72670`) scoped that gate away, so a hot single-pass body is
>   superseded by an IR body (`CRATONVM_C2_SUPERSEDE`, default on) compiled in
>   the background. d7/u's status below already named "the warm IR
>   `caller`"; the d7 census that read `tier=sp` ran with the census env,
>   which slows the run enough that the judged call reached the single-pass
>   body before the IR body was published -- the same race that makes the
>   census env PASS on Generational now (its single-pass site IS d9/d's
>   owned shape) while G1 / ZGC (no census work, so no slowdown) keep the IR
>   body and FAIL. The IR direct call keeps the argument VALUES' homes live
>   across the call (`jit/src/ir_lower.rs` `emit_direct_cross_call`,
>   re-staged after it by `emit_callee_deopt_service_cold`). To confirm on
>   the d9 build: `CRATONVM_C2_SUPERSEDE=0` (single-pass body kept) should
>   read `PASS all 2` on all three collectors, and
>   `CRATONVM_DBG_JIT_DIRECT_BINDS=Gcd1ArgPinProbe.caller` names the bind.
> - **Why neither shape may be extended for this probe:** `caller` calls
>   `make(mb, box)` -- an invoke, which commits -- before `dropAndCheck`. A
>   caller that keeps no argument copy answers a rare callee decline by being
>   re-run from entry, which would run `make` (and anything a real caller did
>   there) twice. Since d10/f's second commit the single-pass rule refuses
>   exactly such sites (`jit/src/x64/op_invoke.rs`,
>   `owned_args_handoff_replays_exactly`;
>   `gcd-d10f-owned-args-handoff-replays-the-caller-unchecked-20260928.md`), so
>   on this build the warm case FAILS on every collector with the census env
>   too. An IR-tier twin of the rule would refuse it for the same reason.
> - **What would fix it:** a call site that never needs the arguments after
>   the call (`gcd-d9d-proposal-no-call-site-rerun-from-entry-20260928.md`:
>   decline arms answered from the callee's own frame, HotSpot's model), not
>   a wider admission.
> - **Expected on this build** (`P="--java-home $JDK <gc> -Xmx64m -cp tools/bench"`,
>   each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`, `-XX:+UseZGC`):
>   `timeout 300 cratonvm $P Gcd1ArgPinProbe` prints `PASS arg-pin-cold-caller`,
>   `FAIL arg-pin-warm-caller`, `FAIL 1 of 2`, rc 1, 3/3 -- with and without
>   the census env and with `CRATONVM_C2_SUPERSEDE=0`;
>   `CRATONVM_JIT_CALLEE_OWNED_ARGS=0` the same. The owned shapes that ARE
>   admitted are measured by `tools/bench/Gcd1ArgPinSpecialProbe.java`
>   (`PASS all 3`). HotSpot prints `PASS all 2`.
>
> **Earlier status (2026-09-28, gcd d9/d, lane args9): FIXED IN CODE for the
> measured holder (unbuilt; default on, `CRATONVM_JIT_CALLEE_OWNED_ARGS=0`
> restores it). Retire when the run below passes; the other call shapes that
> still keep a copy moved to
> `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes-20260928.md`.**
>
> - **The holder was not channel (a) of the IR tier.** The d7 census
>   (`arg_pin_census_1..2`, both runs, every major) names ONE frame word and
>   nothing else: `holder#1 ... cat=11 .../jit-shadow-stack-indirect
>   prov="method=Gcd1ArgPinProbe.caller:(IZ)Z off=104 region=operand-spill
>   tier=sp sp=12 live_hi=112 in_map=true"`, and the census reports "with
>   their objects blocked no root reaches it". `tier=sp`: `caller` is a
>   SINGLE-PASS body, and off=104 is argument 0 (the list) of the baked
>   direct `invokestatic dropAndCheck`'s callee-deopt SERVICE COPY
>   (`jit/src/x64/op_invoke.rs`, `reserve_direct_call_service_slots`: three
>   words at 88..104, cursor 112). The copy was pushed to
>   `pending_staged_arg_oops`, so the call's map named it and the shadow
>   stack published it for the whole call. The operand slot the list was
>   popped from is above the cursor once the copy is gone.
> - **Landed:** a baked single-pass direct `invokestatic` into a body whose
>   method declares no exception table and which has no frameless trap stub
>   keeps NO copy of its arguments -- the callee owns them, as HotSpot's
>   outgoing arguments belong to the callee's frame.
>   `jit/src/x64/op_invoke.rs` (`walk_invokestatic`, direct arm;
>   `sp_direct_call_args_owned_by_callee`, `callee_owned_args_enabled`): no
>   service range, no staged-oop naming, the arguments' registers left out of
>   the call's register mask and spill image.
>   `jit/src/lib.rs` (`direct_callee_owns_its_arguments`,
>   `callee_body_owns_its_arguments`, `SERVICE_ARGS_OWNED_BY_CALLEE`,
>   `CompiledMethod::method_declares_no_handlers` stamped by
>   `try_compile_request`); `jit/src/x64/driver.rs` (the same stamp for every
>   single-pass door); `jit/src/x64/deopt_stubs.rs`
>   (`emit_inline_callee_deopt_check_owned_args`: null buffer, count word
>   `-1`). `vm/src/jit/helpers.rs` (`jit_service_callee_deopt_body` decodes
>   the word; `handle_compiled_callee_deopt_sentinel(.., args_owned_by_callee)`
>   still resumes a stashed callee precisely and passes exceptions through,
>   and hands the three arms that read arguments back -- declined-stash
>   re-run (`drop_declined_callee_stash`), frameless re-run, callee handler --
>   to the caller's caller as a frameless deopt of the caller).
> - **Trade-off, named:** on those three rare arms the REPLAY widens from the
>   callee to the caller (the caller's own caller re-runs it, as for a
>   frameless trap of the caller). The site admission keeps them rare: the
>   callee has no table (so no handler arm), no frameless trap stub, is not
>   synchronized, and the site is unprotected, not a retire cell (every
>   in-loop and every OSR-body `invokestatic`), not caller-synchronized.
>   Each hand-off prints `[cratonvm-deopt] owned-arguments call site: ...`
>   under `CRATONVM_DBG_DEOPT`.
> - **Tests:** `jit/src/tests.rs`
>   `a_direct_call_into_an_owning_body_names_no_argument_copy`;
>   `jit/src/lib.rs` `gcd_d9d_callee_owned_args_tests`;
>   `jit/src/x64/op_invoke.rs` `gcd_d9d_owned_args_site_tests`;
>   `jit/src/x64/deopt_stubs.rs` `gcd_d9d_owned_args_check_tests`.
>
> **Run (orchestrator), Linux release, `-XX:+UseGenerationalGC`:**
> ```
> P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
> for i in 1 2 3; do timeout 300 cratonvm $P Gcd1ArgPinProbe; done
> CRATONVM_JIT_CALLEE_OWNED_ARGS=0 timeout 300 cratonvm $P Gcd1ArgPinProbe   # control
> CRATONVM_DBG=oldmark-root-census,root-source timeout 600 cratonvm $P Gcd1ArgPinProbe 2>argpin.log
> grep -n 'holder#' argpin.log | grep -v 'Static fields' | head
> ```
> Expected: `PASS arg-pin-cold-caller`, `PASS arg-pin-warm-caller`,
> `PASS all 2`, exit 0, 3 of 3 (HotSpot `-XX:+UseSerialGC -Xmx64m` prints
> exactly these, 3 of 3, and 1 of 1 under `-Xint`); the control keeps
> `FAIL arg-pin-warm-caller`, `FAIL 1 of 2` (the d7 output); the census grep
> prints no `holder#` naming `Gcd1ArgPinProbe.caller` or `dropAndCheck`. If
> the warm case still fails, the census names the next holder: a
> `tier=ir` frame of `caller` is the IR tier's channel (a) (the new page), a
> `native-pin` holder the interpreter / Rust-door pins (the new page too).
> Also run the deopt / re-run battery and `GenR4W6JitOomRootProbe` (3/3 on
> an idle host) to check the propagated arms.

> **Earlier status (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): OPEN, unchanged -- channel (a).** `Gcd1ArgPinProbe -XX:+UseGenerationalGC -Xmx64m` prints `PASS arg-pin-cold-caller`, `FAIL arg-pin-warm-caller`, `FAIL 1 of 2` in all nine d7 runs (`arg_pin_1..3`, `arg_pin_census_1..2`, `jit8_argpin_1..3`, `jit8_argpin_census`) and with `CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS=0` (`jit8_argpin_ctl`), while the same build passes the OsrDeadSlot probe's identical `cleared` 6/6. **Remaining gate unchanged:** `gcd-d4o-proposal-callee-owned-arguments-20260928.md` lands, then HotSpot's `PASS all 2` 3/3.

> **STATUS (2026-09-28, gcd d7/u): OPEN; what is left is channel (a) alone,
> and its fix is the proposal, not a defect fix.** Measured on the d6 build
> (`feaec464b`): `Gcd1ArgPinProbe` still `PASS arg-pin-cold-caller`, `FAIL
> arg-pin-warm-caller`, `FAIL 1 of 2`, 3/3 -- with d6/u's value-range fix in,
> which (same build) makes the OsrDeadSlot probe's identical `cleared` pass
> 3/3. So the warm case's remaining holder is (a): the warm IR `caller`'s
> home of `make(..)`'s result, which is an ARGUMENT of the `dropAndCheck`
> call and therefore a value read at that call. No dead-home clear may zero it
> before the call, and it must stay while the callee runs, because every
> re-run arm (the callee-deopt service, `try_resume_trapped_callee`) reads the
> arguments back from those words after the call returns. HotSpot does not
> keep it: its outgoing arguments belong to the callee's frame, whose oop map
> drops the dead parameter. The fix is
> `gcd-d4o-proposal-callee-owned-arguments-20260928.md` (callee-owned argument
> words, with the re-run arms rewritten to re-read from the callee's
> materialised frame instead of the caller's homes). Channels (b) and (c) are
> not implicated by any measurement. **Keep open until the proposal lands;
> no probe change is expected before then.**

> **Earlier status (2026-09-28, gcd d6/u): OPEN. Still `FAIL arg-pin-warm-caller` 3/3
> on the d5 build. The `cleared` half is the SAME gap as OsrDeadSlot's
> (`cleared` is the same method shape, compiled eagerly as a callee of the
> warm `dropAndCheck`): its `ref.get()` call result's home is kept at its own
> `System.gc()` because the slot plan counted that call's MEMORY-token edge
> as a read. Fixed this wave (`jit/src/ir_lower.rs::value_use_ranges`,
> default on, `CRATONVM_JIT_IR_DEAD_HOME_VALUE_RANGES=0` off; see the
> OsrDeadSlot page's d6/u block). By reading, channel (a) below then remains
> -- `caller`'s home of `make(..)`'s result is an ARGUMENT of the
> `dropAndCheck` call, a value read at that call, so no dead-home clear may
> touch it while the callee runs (the re-run arms read it back) -- so this
> probe may still fail after the fix; its census then names (a).**
>
> **Run (orchestrator):**
> ```
> P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
> for i in 1 2 3; do timeout 300 cratonvm $P Gcd1ArgPinProbe; done
> CRATONVM_DBG_IR_SLOTS=1 CRATONVM_DBG=oldmark-root-census,root-source \
>   timeout 600 cratonvm $P Gcd1ArgPinProbe 2>argpin.log
> grep -n -A60 'method Gcd1ArgPinProbe.cleared' argpin.log | grep -E 'homes|value range|keep n|dead-home clears'
> grep -n 'holder#1' argpin.log | head
> ```
> Expected (HotSpot): `PASS arg-pin-cold-caller`, `PASS arg-pin-warm-caller`,
> `PASS all 2`. In `cleared`'s lines, as for OsrDeadSlot: a `value range` line
> for the `ref.get()` call at bci 8 ending before the `System.gc()` call's
> position, that colour in the `System.gc()` site's `dead [...]`, and its
> home in that site's `dead-home clears ... offsets`. If the probe still
> fails, `holder#1` names the channel: `region=operand-spill` at `caller` =
> (a) (the fix is `gcd-d4o-proposal-callee-owned-arguments-20260928.md`),
> `dropAndCheck` = (b), `native-pin` = (c).

> **Earlier status (2026-09-28, gcd d5/u): OPEN; the `cleared` holder has a second
> fix (unbuilt). Measured on `d916d1c40` (orchestrator): still `PASS
> arg-pin-cold-caller`, `FAIL arg-pin-warm-caller`, `FAIL 1 of 2`, 3/3, so
> d4/o's union kill did not remove the IR `cleared` home.**
>
> - **Why, by reading:** the value `cleared`'s home keeps is the guarded
>   splice's result PHI of `ref.get()`, not the spliced load d4/o's kill
>   handles; see the d5/u block of
>   `../../internal/gc/gengc-r5w3-live7-osr-dead-slot-holder-is-outside-the-osr-frame-FIXED-20260928.md`.
>   The fix is the same (`jit/src/ir_lower.rs`, `phi_entry_kills`, default
>   on, `CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS=0` off).
> - **This page's own channels (a)-(c) below are unchanged.**
>
> **Run (orchestrator):**
> ```
> P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
> for i in 1 2 3; do timeout 300 cratonvm $P Gcd1ArgPinProbe; done
> CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS=0 timeout 300 cratonvm $P Gcd1ArgPinProbe   # control
> CRATONVM_DBG_IR_SLOTS=1 CRATONVM_DBG=oldmark-root-census,root-source \
>   timeout 600 cratonvm $P Gcd1ArgPinProbe 2>argpin.log
> grep -n 'holder#1' argpin.log | head; grep -n 'ir-slots\] method\|phi-entry kill' argpin.log | head -40
> ```
> Expected (HotSpot): `PASS arg-pin-cold-caller`, `PASS arg-pin-warm-caller`,
> `PASS all 2`, exit 0, 3/3 if the IR `cleared` home was the only holder;
> the control keeps `FAIL 1 of 2`. Otherwise the `holder#1` line names the
> next channel: `region=operand-spill` at `caller` = (a), `region=java-local`
> or a parameter home at `dropAndCheck` = (b), `cat=.../native-pin` = (c).

> **Earlier status (2026-09-28, gcd d4/o): OPEN. The probe line that fails is
> `FAIL arg-pin-warm-caller`, `FAIL 1 of 2` (d2 tip, 3/3; `PASS
> arg-pin-cold-caller`; HotSpot `PASS all 2`). The likeliest holder is NOT a
> pin but the one the OsrDeadSlot census named, and its fix landed this
> wave. No code change on this page's pins.**
>
> - **The warm case's `cleared` is the same method as OsrDeadSlot's.** It is
>   called for the first time from a COMPILED `dropAndCheck` (the warm-up
>   passes `check=false`, so `cleared` never runs until then). The IR compile
>   of `dropAndCheck` compiles its statically bound callee `cleared` eagerly
>   too. That is exactly the IR `cleared` whose `ref.get()` home (`off=160`)
>   the OsrDeadSlot census names as the holder across its own
>   `System.gc()`. The cold case runs `cleared` interpreted and passes. Fix:
>   `jit/src/ir_lower.rs`, `refine_reach_by_def_kills` (default on,
>   `CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS=0`); see
>   `../../internal/gc/gengc-r5w3-live7-osr-dead-slot-holder-is-outside-the-osr-frame-FIXED-20260928.md`.
> - **Still rooted after that, by reading (this page's own items, unfixed):**
>   (a) the IR caller's home of the argument value (`make(..)`'s result),
>   which is an input of the `dropAndCheck` call, so no dead-home clear can
>   zero it before the call stages it, and the frame block publishes it for
>   the whole call; (b) `dropAndCheck`'s own parameter home for `l`, which no
>   clear ever touches (oomjit9 item 5 (a)); (c) the Rust-door pins, if the
>   call goes through a dispatch helper. HotSpot keeps none of the three: a
>   dead outgoing argument is the callee's, and the callee's map drops a dead
>   parameter. If the probe still fails after the fix, the census line
>   decides which: `cat=.../native-pin` = (c); `region=java-local` at
>   `dropAndCheck` = (b); `region=operand-spill` at `caller` = (a).
>   Proposed fixes and their hazards (every re-run arm reads the arguments
>   back, so (a) and (c) need the re-run arms rewritten, not just zeroed):
>   `gcd-d4o-proposal-callee-owned-arguments-20260928.md`.
>
> **Run (orchestrator):**
> ```
> P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
> for i in 1 2 3; do timeout 300 cratonvm $P Gcd1ArgPinProbe; done
> CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS=0 timeout 300 cratonvm $P Gcd1ArgPinProbe   # control
> CRATONVM_DBG=oldmark-root-census,root-source CRATONVM_DBG_JIT_ROOTSCAN=1 \
>   timeout 600 cratonvm $P Gcd1ArgPinProbe 2>argpin.log; grep -n 'holder#1' argpin.log | head
> ```
> Expected (HotSpot): `PASS arg-pin-cold-caller`, `PASS arg-pin-warm-caller`,
> `PASS all 2`, exit 0, 3/3 if the IR `cleared` home was the only holder.

> **Earlier status (2026-09-27, gcd d3/o, family consolidation): OPEN, unchanged in
> code; this page is the family's one home for call-argument pins. Its only
> probe line is UNMEASURED:** `Gcd1ArgPinProbe` (d2/f) has not been run yet,
> so no line is known to fail. Re-read on `e352a1d35`: `JitArgPinGuard`
> (`vm/src/runtime/interpreter/jit_bridge.rs`) and the `CompileArgPins`
> rerun pins (`vm/src/jit/helpers.rs`) still hold every reference argument
> for the whole call, for the re-run arms. Not the holder of the umbrella's
> `GenR4W4NativeStringOomProbe` line 77 by reading (no call passing `fill`
> is in flight there). **Run:** the d2/f block's three commands. Expected:
> HotSpot's `PASS arg-pin-cold-caller`, `PASS arg-pin-warm-caller`,
> `PASS all 2` (exit 0); a JIT `FAIL` with a `cat=.../native-pin` holder
> confirms the page, a JIT `PASS` 3 of 3 retires it as unreproducible.

> **Earlier status (2026-09-27, gcd d2/f): OPEN, unchanged in code; the page's probe
> now EXISTS, so the next run measures the defect instead of arguing it.**
>
> - **Landed:** `tools/bench/Gcd1ArgPinProbe.java`, the `GenR5W4ArgPinProbe`
>   this page specified (the `Gcd1` prefix is this round's probe convention).
>   A caller passes a 16 MB list that no local of its own holds (`make`
>   builds it and parks its `WeakReference` in a one-element array) to
>   `dropAndCheck(l, box, check)`, whose body nulls `l` and then polls the
>   reference through `System.gc()`. There are two cases: a cold caller, and
>   a caller warmed with empty lists so that the call is compiled to
>   compiled. HotSpot prints the three lines below in 3 of 3 runs,
>   `-XX:+UseSerialGC -Xmx64m`, and in 1 of 1 run with `-Xint`.
> - **Re-read, unchanged:** `JitArgPinGuard` (`jit_bridge.rs`) and the rerun
>   pins (`CompileArgPins` in `helpers.rs`) still pin every reference
>   argument for the whole call, because the `Throw`/`Stashed`/declined
>   arms read them back after the call. A declined decoded door rebuilds
>   the interpreted frame from the caller's buffer
>   (`write_back_pinned_decoded_args`), so an early release would hand it a
>   vacated address. Fix 1 below (resume precisely) is still the direction.
>
> **Run (orchestrator):**
> ```
> P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
> javac -d tools/bench tools/bench/Gcd1ArgPinProbe.java
> timeout 300 cratonvm $P Gcd1ArgPinProbe
> timeout 300 cratonvm $P --nojit Gcd1ArgPinProbe
> CRATONVM_DBG=oldmark-root-census,root-source CRATONVM_DBG_JIT_ROOTSCAN=1 \
>   timeout 600 cratonvm $P Gcd1ArgPinProbe 2>argpin.log; grep -n 'holder#' argpin.log | head
> ```
> Expected (HotSpot, and what retires this page): `PASS arg-pin-cold-caller`,
> `PASS arg-pin-warm-caller`, `PASS all 2`, exit 0. With the JIT this page
> predicts `FAIL` with a `cat=.../native-pin` holder. A FAIL whose holder is
> NOT `native-pin` is some other frame word, so read its `prov=`. A PASS
> with the JIT retires this page as unreproducible.

> **Earlier status (2026-09-27, gcd d1/b): OPEN, unchanged in code.** Re-read
> against `6d39e8dcc`: `JitArgPinGuard` (`jit_bridge.rs`) and the
> `CompileArgPins` rerun pins (`helpers.rs`) still pin every reference
> argument for the whole call, for the re-run arms. No failing probe of this
> round is shaped like it (every `GenR4W6JitOomRootProbe` / OsrDeadSlot check
> passes its list through a static or a returned frame), so it was not
> worked; fix 1 of the page (resume precisely instead of re-running) is the
> direction, fix 3 the cheap lever. The probe the page specifies
> (`GenR5W4ArgPinProbe`) is still unwritten; its census signature is a
> `cat=<step>/native-pin` holder.

> **Earlier status (2026-09-26, gen r5w4/jit8): OPEN, filed from reading. No code
> change.** A retention of the JIT alone (`--nojit` has no such root): an
> argument the compiled callee has stopped using -- or nulled -- survives
> every collection the callee runs, because the dispatch that called it pins
> the argument words for the whole call so a RE-RUN of the callee can read
> them back.

*Filed 2026-09-26 by gen round 5 wave 4, lane `jit8`, during the adversarial
review of `vm/src/runtime/interpreter/jit_bridge.rs` and
`vm/src/jit/helpers.rs`.*

- **Severity:** retention, MEDIUM. Bounded by the call: the pins go when the
  call returns. What it costs is every dropped structure a long-running
  compiled callee received as an argument (a worker loop handed a batch it
  drops, a method that nulls its parameter and then allocates to an OOME or
  polls a `WeakReference`). HotSpot's compiled frame drops a dead parameter
  from its oop map at the next safepoint; the interpreter here drops a dead
  local through its per-bci liveness filter.
- **Where:**
  - interpreter -> compiled: `vm/src/runtime/interpreter/jit_bridge.rs`,
    the argument pop loop before `run_jit_body` (search
    `JitArgPinGuard { thread: thread as *mut JvmThread, base: args_pin_base }`):
    every non-null reference argument is pushed onto `native_pin_roots` and the
    guard releases them when the call RETURNS. They are read back only on the
    `JitBodyOutcome::Throw` arm (`jit_saved_args_to_values_pinned`, to seed the
    handler frame) and the `Stashed` arm (a re-run from entry).
  - compiled -> compiled through a Rust dispatch door:
    `vm/src/jit/helpers.rs`, every `CompileArgPins::pin_raw` taken with
    `rerun_pins` / `callee_rerun_pins()` / `root_args`
    (`try_mic_rust_cached_entry`, the MIC and megamorphic hit arms of the
    virtual dispatch, `call_compiled_then_route`): pinned across the call
    because the callee's `i64::MIN` sentinel can re-run it with these
    arguments.
- **Not** the holder of `GenR5W3OsrHolderProbe` / `GenR5W2OsrDeadSlotProbe`
  by reading: no live call at their checks passes the dropped list.

## Why the pins exist, and why they cannot simply go

A callee re-run (from entry, or at its own handler) needs the ORIGINAL
arguments at their CURRENT addresses: the call's staging words are in no oop
map, so after a moving collection only the pins say where the objects went
(`r11w4-rt-callee-rerun-reads-pre-call-argument-words-FIXED-20260924.md`).
Dropping them early would make the re-run read a vacated address. So the fix
is to stop needing them, not to release them.

## Proposed fix (in order of preference)

1. **Resume precisely instead of re-running.** Every re-run arm exists because
   the callee's frame could not be rebuilt at its trap point. Where the
   callee's frame IS rebuilt (`try_resume_trapped_callee`,
   `resume_real_ir_deopt`, `run_jit_callee_handler`), the rebuilt frame roots
   its own parameters by liveness and the call's pins are redundant. A census
   of which arm each re-run takes (`CRATONVM_DBG=deopt`) says how much of the
   pinning is spent on paths that never re-run.
2. **Pin the callee's arguments in its OWN frame, not in the caller's.** The
   optimizing tier already publishes `ref_param_homes`; a callee that trapped
   can hand its parameter homes to the sink (the stash carries them) and the
   dispatch then needs no pin -- the re-run reads the parameters the callee's
   frame kept, which its map drops once dead (item 2 of
   `../../internal/gc/gengc-r5w2-oomjit6-compiled-frame-residue-residuals-RETIRED-20260928.md`).
3. **As a lever:** an opt-in that releases the interpreter-side pins at the
   compiled body's first safepoint poll that proves no re-run is possible
   any more (a callee that has passed a side-effecting call can only resume,
   never re-run). Needs a per-body "re-run fence" fact from the emitter.

## How to verify

A probe (to be written as `GenR5W4ArgPinProbe` under `tools/bench`): an interpreted
caller (the method runs once) passes an 8 MB `ArrayList` to a warmed,
compiled `static boolean dropAndCheck(List<long[]> l, WeakReference<?> ref)`
whose body is `l = null; return cleared(ref);`. HotSpot (`-XX:+UseSerialGC
-Xmx64m`) and `--nojit` print `PASS arg-pin`; this build is expected to print
`FAIL arg-pin` with the JIT, and the holder census
(`CRATONVM_DBG=oldmark-root-census`) to print `holder#1 ... cat=<collect_roots step>/native-pin`.
Retire when the JIT arm prints `PASS arg-pin` and the census names no
`native-pin` holder.
