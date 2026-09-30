# A compiled callee's exceptional frame can outlive the catch that ended its exception

> **STATUS (2026-09-29, gce e2/f): NARROWED -- probes for items 1 and 3
> written; no retention on Generational or ZGC.**
> `tools/probes/GceE2fLocalCatchOrphanFrameProbe.java`: a compiled
> `catcherDirect` (item 1) and a compiled `catcherInterface` calling through
> an interface (item 3's dispatch-resumed shape) each catch, in their OWN
> handler, an exception a compiled callee threw past its non-matching
> handler while naming a 16 MiB local; a weak reference to the array must
> then clear. HotSpot 25.0.3 (`-XX:+UseSerialGC`, `-Xint`, `-Xmx64m`):
> `PASS local-catch-direct`, `PASS local-catch-interface`, `PASS all 2`,
> rc 0. Base binary (Windows): Generational and ZGC = HS; G1 `FAIL
> local-catch-interface` 1/1 (out of this lane's scope; G1 backend). The
> deopt-stash census on the Generational run (`CRATONVM_DBG=oldmark-root-census,excframe`):
> `callee`'s precise frames are published (`FRAME method=...callee ...
> RegisterRef(13)`) and no door reports `stash_refs>0` after the catch.
> **Rows:** `p ${gc}_localcatch_$r 300 "" "$X -Xmx64m" GceE2fLocalCatchOrphanFrameProbe`
> and `p gen_localcatch_census 300 "CRATONVM_DBG=oldmark-root-census" "-XX:+UseGenerationalGC -Xmx64m" GceE2fLocalCatchOrphanFrameProbe`
> (pass bound: no `compiled local handler: ... stash_refs=` line with a
> non-zero count). If Generational and ZGC are HS 3/3 with the osrorphan rows
> (already 9/9), re-file this page as a known gap per "Retire when" (the G1
> line goes to a G1 page).

> **STATUS (2026-09-29, gce e1/x): KEEP -- item 2's plain shape does not retain on Linux either; the retire condition needs the census.** `GceE1fOsrOrphanExceptionalFrameProbe -Xmx64m` prints `caught=true` / `PASS osr-orphan-frame` 9/9 on Generational, G1 and ZGC on e1 and on base (`verify-e1/ve1`, `*_osrorphan_1..3`). **Remaining:** per "Retire when", the `vm/deopt-stash` census at zero old bytes on the wave-4 probes and on a probe of item 1's direct-call shape; item 3 (a dispatch-resumed caller) has no probe. Then re-file as a known gap.

> **STATUS (2026-09-29, gce e1/f): NARROWED -- the probe items 2 and 3 asked
> for is written, and item 2's shape does NOT retain on the base.**
> `tools/probes/GceE1fOsrOrphanExceptionalFrameProbe.java`: an OSR'd `loop`
> calls a compiled `callee` that throws `IllegalStateException` from inside a
> `try` whose only handler (`ArithmeticException`) reads a 16 MiB local; the
> exception leaves `callee` and the OSR body and is caught in `main`, then a
> weak reference to the array must clear. HotSpot 25.0.3 (`-XX:+UseSerialGC`
> and `-Xint`, `-Xmx64m`): `caught=true`, `PASS osr-orphan-frame`, rc 0. The
> base binary (`adb9178bc`, Windows, Generational): the same, 2/2, and 1/1
> with `CRATONVM_JIT_OSR_DROP_ORPHANS=1`; `CRATONVM_DBG_JITC=1` shows the OSR
> compile of `loop` and the retire-cell call into the compiled `callee`, and
> `CRATONVM_DBG=excframe` shows `callee`'s precise frame naming the array
> (`bci=37/41 ... RegisterRef(13)`). The census's doors report
> `stash_refs=0`: the frame is claimed by the call site's service
> (`run_jit_callee_handler` -> `precise_handler_frame_for`) before the OSR
> sink can re-stash it. So item 2 has no reproducer in its plain shape;
> item 3 (a dispatch-resumed caller) is untested. **Run (each collector):**
> `javac -d tools/probes tools/probes/GceE1fOsrOrphanExceptionalFrameProbe.java`,
> `cratonvm <gc> -Xmx64m -cp tools/probes GceE1fOsrOrphanExceptionalFrameProbe`
> x3: HotSpot's two lines, rc 0. If it passes on Linux on all three, this page
> can be re-filed as a known gap per its "Retire when".

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): unchanged -- OPEN, no probe line fails because of it.** `GenR4W6JitOomRootProbe`'s `orphan-exceptional-frame` passes in every d7 default run (`foome_jitoomroot_1..5`, `jit_oom_root_1..3`), and `CRATONVM_JIT_OSR_DROP_ORPHANS=1` changes nothing on the pinned-callee probe (`pcallee_orph_1..3` fail like the default). Items 2 and 3 still want a probe that throws through an OSR'd or dispatch-resumed caller from a callee with a non-matching handler; the fix once proposed there (`../../internal/gc/gengc-r5w1-oom5-proposal-exceptional-stash-floor-at-every-door-REJECTED-20260928.md`) was rejected for want of a reproducer, so the probe comes first.

> **STATUS (2026-09-27, gcd d3/o, family consolidation): OPEN, unchanged in
> code; the family's one home for exceptional-stash orphans. No probe line
> fails because of it.** Re-read on `e352a1d35` for the umbrella's
> `GenR4W4NativeStringOomProbe`: not its holder by reading. The phase-1
> route TAKES `main`'s own reason-9 frame, and a take discards every frame
> beneath it (`jit/src/deopt.rs`, `pop_stash_entry`). A FOREIGN top would
> have been re-stashed with no precise frame, and the OOME would then have
> escaped `main` in phase 1, which the measured output contradicts. Items 2
> and 3 still want a probe that throws through an OSR'd or dispatch-resumed
> caller from a callee WITH a non-matching handler; the fix is still the
> proposal `../../internal/gc/gengc-r5w1-oom5-proposal-exceptional-stash-floor-at-every-door-REJECTED-20260928.md`.
> **Run:** none new; the umbrella's `CRATONVM_JIT_OSR_DROP_ORPHANS=1` arm
> passing would put the umbrella's line back here.

> **Earlier status (2026-09-27, gcd d2/f): OPEN, unchanged in code. It is still not
> the holder of any failing probe.** Re-read on `a1fa77603`:
> `route_osr_exception_out_of_artifact` and `try_osr`'s entry floor
> (`vm/src/runtime/interpreter/jit_bridge.rs`) are as the d1/b block
> describes, and `LAST_EXCEPTIONAL` (`jit/src/deopt.rs`) is still truncated
> only by the opt-in `CRATONVM_JIT_OSR_DROP_ORPHANS=1` and by the
> method-entry sink's foreign-frame drop. The census now labels the stash's
> holders `deopt-stash` as before. A holder with that label after a caught
> exception is this page. Items 2 and 3 still need a probe that stages a
> callee with a non-matching handler, thrown through an OSR'd or
> dispatch-resumed caller. The general fix is still the proposal
> `../../internal/gc/gengc-r5w1-oom5-proposal-exceptional-stash-floor-at-every-door-REJECTED-20260928.md`.
> No flip: making the drop default is the orchestrator's call.

> **Earlier status (2026-09-27, gcd d1/b): OPEN, unchanged in code; still not the
> holder of any failing probe.** Re-read against `route_osr_exception_out_of_artifact`
> (`vm/src/runtime/interpreter/jit_bridge.rs`): a foreign frame is still
> re-stashed and only the opt-in `CRATONVM_JIT_OSR_DROP_ORPHANS=1` truncates
> above the entry floor; nothing flipped (a default flip is the
> orchestrator's call). `GenR4W4NativeStringOomProbe` -- the probe this page
> asked to re-run with that flag -- has, by reading, a different cause (the
> JIT helpers' latched overhead limit, fixed this round, umbrella page); if
> it still fails after that fix, run it once with
> `CRATONVM_JIT_OSR_DROP_ORPHANS=1 CRATONVM_DBG=oldmark-root-census` and look
> for `dropped N orphaned exceptional frame(s)` (N > 0 puts it back here).
> Items 2 and 3 remain real leaks for a callee with a non-matching handler
> thrown through an OSR'd or dispatch-resumed caller; no probe stages that
> shape yet.

> **Earlier status (2026-09-26, gen r5w2/oomjit6): UNCHANGED in code; re-judged as
> NOT the holder of the regressed probes.** Walked for the wave's item 1:
> `GenR4W6JitOomRootProbe`'s `catch-staged-arg` publishes no exceptional
> frame at all (`stagedArgCatchThenCheck` is not RBC.6, so its optimizing body
> has no reason-9 pads, and `touchAndThrow` has no handler), and
> `orphan-exceptional-frame`'s callee frame reaches the caller's
> `route_jit_signal_exception`, which drops a foreign frame. Those shapes are
> held by the optimizing OSR frame's dead words instead
> (`../../internal/gc/gengc-r5w2-oomjit6-ir-frames-keep-dead-references-as-roots-FIXED-20260928.md`).
> Items 2 and 3 remain real leaks for a callee WITH a non-matching handler
> that throws through an OSR'd or dispatch-resumed caller; the general fix is
> still `../../internal/gc/gengc-r5w1-oom5-proposal-exceptional-stash-floor-at-every-door-REJECTED-20260928.md`,
> not implemented this wave (it threads a floor through every JIT door in
> `helpers.rs` and needs a probe that stages the nested-sink case).
>
> **Probe:** unchanged below (`CRATONVM_JIT_OSR_DROP_ORPHANS=1` on
> `GenR4W4NativeStringOomProbe`; a census `dropped N orphaned exceptional
> frame(s)` line would put that probe back on this page).

> **Earlier status (2026-09-26, gen r5w1/oom5): item 2 FIX LANDED opt-in
> (`CRATONVM_JIT_OSR_DROP_ORPHANS=1`); item 3 OPEN with its design below;
> item 1's opt-in is NOT safe to make default (it SIGSEGVs on dev, per the
> orchestrator).**
>
> - **Item 2 (the OSR sink):** `try_osr` records
>   `cratonvm_jit::deopt::exceptional_stash_depth()` just before it enters the
>   artifact, and once the body has returned, `route_osr_exception_out_of_artifact`
>   (every exception drain) and the sentinel arm truncate the stash to that
>   floor (`truncate_exceptional_stash_to`). Every frame above it was
>   published inside the activation by a compiled frame that has unwound; an
>   outer sink's frame published before the entry is below it and survives
>   (the case the re-stash protects). Same argument, and the same under-drop
>   property, as oomjit6's compiled-catch floor, but recorded locally in
>   `try_osr`: it does not use `JitEntryGuard`'s floor bookkeeping (the part
>   that SIGSEGVs). The census prints
>   `[oldmark-root-census] OSR sink <method>: dropped N orphaned exceptional frame(s) above the entry floor F`
>   when it acts. Opt-in because it removes GC roots on the path every
>   caught exception inside an OSR'd loop takes; the sibling method-entry
>   sink (`route_jit_signal_exception`) already drops every foreign frame by
>   default, which is a superset.
> - **Item 1 verdict (`CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS`):** the floor
>   argument is sound, but (a) the orchestrator measured a SIGSEGV in
>   `vm/src/jit/conservative_roots.rs` with it alone on dev `9e252c8b2`
>   (lane `young5`'s file), and (b) it acts only at a single-pass compiled
>   local handler. Since JIT r11 wave 13 the optimizing tier resumes its
>   catches in the interpreter (`ir-precise-handler-frames` default on), so it
>   cannot reach those. Do not default it.
> - **Item 3 (`precise_handler_frame_for`, `drop_own_exceptional_frame`):**
>   the same floor, recorded at the door that entered the compiled callee
>   (`run_jit_body_raw` / the dispatch helpers that call
>   `run_jit_callee_handler`), truncated after the callee handler has claimed
>   or re-stashed its frame. Not landed: it threads a value through every
>   JIT-to-JIT dispatch door in `vm/src/jit/helpers.rs` and
>   `exception_dispatch.rs` and wants its own probe; see
>   `../../internal/gc/gengc-r5w1-oom5-jit-oome-retention-regressed-on-dev-9e252c8b2-FIXED-20260928.md`,
>   candidate 1, for why it is now the likeliest root of two probes.
>
> **Probe:** `CRATONVM_JIT_OSR_DROP_ORPHANS=1 CRATONVM_DBG=oldmark-root-census timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -cp tools/bench GenR4W4NativeStringOomProbe`
> must print `fill: OutOfMemoryError "Java heap space"`, `native-strings ok`,
> `recovered ok`, `PASS` if the OSR sink's re-stash is the root (the census
> then shows `dropped N orphaned exceptional frame(s)` lines). The same flag on
> `GenR4W6JitOomRootProbe`: compare `oome-osr-loop` and `catch-staged-arg`
> with and without it.

*Filed 2026-09-24, generational GC round 4 wave 5, lane `oomjit5`, from
reading. Found while walking every structure that can hold a compiled frame's
locals after its exception leaves it
(`docs/internal/gaps/gengc-r4w4-final-oome-from-compiled-code-leaves-dropped-data-reachable-20260924.md`).*

- **Status:** item 1 (the compiled local-handler commit): FIX LANDED behind
  the opt-in `CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS`, awaiting probe:
  `CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS=1 CRATONVM_DBG=oldmark-root-census timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -cp tools/bench GenR4W6JitOomRootProbe`
  prints `PASS orphan-exceptional-frame` (and HotSpot's nine lines), and the
  census log shows `dropped N orphaned exceptional frame(s)` lines. Items 2
  and 3 (the OSR sink's re-stash, `precise_handler_frame_for`,
  `drop_own_exceptional_frame`): OPEN. See "Wave 6" at the end.
- **Severity:** retention. Every object a stale frame names stays a GC root
  until the next take of the stash on that thread.
- **Not the root of the wave-4 probes.** `GenR4W4HeapFullThrashProbe`'s `grow`
  has no exception table. Its allocation guard (`emit_post_alloc_oom_check` in
  `jit/src/x64/deopt_stubs.rs`) therefore takes the shared sentinel exit and
  publishes nothing: `precise_exc_stub` needs `pc_is_protected(throw_bci)`.
  The catching frame's own reason-9 frame is TAKEN by its sink.

## What holds the frame

`LAST_EXCEPTIONAL` (`jit/src/deopt.rs`) is a per-thread stack of
`ReconstructedFrame`s. A compiled method publishes one at a throwing site
inside one of its OWN protected ranges when its local handler does not catch.
`for_each_stashed_deopt_object` roots every object every stashed frame names
(`vm/src/memory/roots.rs` step 10, both peer deposits), for as long as it is
stashed. A frame leaves the stash only on:
- a take, which also discards everything beneath it;
- `clear_exceptional_frame_of` naming it exactly.

## Where one is left behind

1. **The compiled local-handler commit.** `jit_local_handler_lookup`
   (`vm/src/jit/helpers.rs`) takes the pending throwable and enters the
   handler. It never looks at `LAST_EXCEPTIONAL`.

   Take a compiled callee `C`, called directly (compiled to compiled), whose own
   `catch` does not match. `C` publishes its frame on its miss edge and returns
   the sentinel. If the compiled caller then catches locally, `C`'s frame is
   stashed with no consumer left. `C` is gone, its caller did not need it, and
   nothing takes it until the next exceptional deopt on the thread.

2. **The OSR sink re-stashes a foreign frame.**
   `route_osr_exception_out_of_artifact` (`vm/src/runtime/interpreter/jit_bridge.rs`)
   takes the top frame, and if it names another method it re-stashes it. When
   the exception then propagates out of the OSR'd method, the callee's frame
   stays.

   Its sibling sink disagrees. `route_jit_signal_exception`
   (`vm/src/runtime/interpreter/exception_dispatch.rs`) DROPS a foreign frame,
   and argues why: "an exceptional frame's owner is always the compiled body
   that just unwound to produce this exception, so if it does not name the
   method being drained here, its owner is gone".

3. **`precise_handler_frame_for`** (same file) and
   **`drop_own_exceptional_frame`** (the first-call tier-up door in
   `vm/src/runtime/interpreter.rs`) re-stash or leave a callee's frame too, for
   the same reason as item 2.

## Why no fix yet

The re-stash in item 2 protects one real case: a sink that has published its
own frame and then runs Java before draining it. That Java enters an OSR'd
loop whose throw published nothing, so the frame on top is the outer sink's.
`vm/tests/deopt_stash_roots_wired.rs` records that an unconditional clear in
this family once broke RBC.6
(`docs/internal/fixed-bugs/rbc6-getfield-putfield-npe-escape-FIXED-20260818.md`).
So "drop every foreign frame" is not safe as stated.

A precise rule needs the stash top from when the catching activation was
ENTERED. Frames published since then are this throw's orphans; the frame that
was already on top belongs to someone else.
- **OSR sink:** record `peek_exceptional_identity()` before `try_osr` enters
  the artifact, and pass it to `route_osr_exception_out_of_artifact`. Drop a
  foreign top that differs from it; re-stash one that matches.
- **Local-handler commit:** the same floor, published per JIT door in a
  thread-local that each door saves and restores. The commit pops tops
  (`clear_exceptional_frame_of`) until the top is the floor. This touches every
  JIT door, so it wants its own lane, with a probe that stages the nested-sink
  case above.

## Detector (landed)

Under `CRATONVM_DBG=oldmark-root-census`, every compiled-catch door prints a
line when it enters a handler with objects still in either stash. Every
consumer that owned them has run by then. The drain in
`drain_native_return_at_compiled_catch` prints it:

```
[oldmark-root-census] compiled local handler: tid=1 stash_refs=3 top_exceptional=Some(("Foo.bar(I)V", 17)) top_deopt=None native_pins=0 ...
```

The old-gen census attributes what such frames keep alive to `vm/deopt-stash`.

## Retire when

Either:
- the census shows `vm/deopt-stash` at zero old bytes on the wave-4 probes and
  on a probe that stages the direct-call shape of item 1, and this is
  re-filed as a known gap; or
- the entry-floor rule lands with that probe.

## Wave 6 (lane `oomjit6`, 2026-09-24): the entry floor, for item 1

Review: `docs/internal/reviews/gengc-round4-w6-oomjit6-20260924.md`.

**The floor.** Every Rust-to-compiled transition constructs a
`JitEntryGuard` (`vm/src/jit/conservative_roots.rs`). Under
`CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS=1` each guard records the current
exceptional-stash depth (`cratonvm_jit::deopt::exceptional_stash_depth`) as
the floor of its span in a thread-local, saves the enclosing door's floor and
restores it on drop. Off, the guard records nothing.

**The drop.** The two commit points of `jit_local_handler_lookup`
(`vm/src/jit/helpers.rs`, the dispatched-throwable arm and
`local_handler_enter_implicit`) call
`drop_orphaned_exceptional_frames_at_compiled_catch`, which truncates the
stash to the innermost floor (`cratonvm_jit::deopt::truncate_exceptional_stash_to`).

**Why it drops only this throw's orphans.** Between the innermost door and
the catching frame there is only compiled code, and a compiled frame
publishes only on its way OUT, so every frame at or above the floor was
published by a frame that has already unwound into this catch. The case this
page's "Why no fix yet" protects -- a sink that publishes its own frame and
then runs Java -- is below the floor: that Java enters compiled code through
a door of its own, which records the deeper floor. A take or a restash inside
the span only lowers what sits above the floor (a take clears the whole stash,
a restash pushes at index 0), and `MAX_STASH_DEPTH`'s `remove(0)` shifts
frames DOWN past the floor, so every error is an under-drop. The
interpreter-side catch doors (`exception_dispatch.rs`, `jit_bridge.rs`) do NOT
drop: their innermost door may be an outer one.

**Opt-in** because it removes roots and has not been measured on the
regression suite. The probe that would justify the default:
`GenR4W6JitOomRootProbe`'s `orphan-exceptional-frame` failing without the
flag and passing with it, plus a clean regression suite and RBC.6's
`vm/tests/deopt_stash_roots_wired.rs` with the flag set.

**Tests:** `cratonvm_jit::deopt::stash_nesting_tests::exceptional_stash_truncation_keeps_the_frames_below_the_floor`;
`vm` `jit::conservative_roots::tests::the_exceptional_stash_floor_nests_and_restores`
and `the_exceptional_stash_floor_is_not_kept_without_the_opt_in`.

**Still open:** items 2 and 3 (the OSR sink's re-stash and its two
siblings). The OSR sink half of the rule is the same floor, recorded before
`try_osr` enters the artifact; that lives in
`vm/src/runtime/interpreter/jit_bridge.rs`, outside this lane.
