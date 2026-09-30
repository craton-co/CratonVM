# Proposal: a compiled activation of a renumbered class leaves at its constant-pool sites instead of translating them

**Status: proposal — filed 2026-10-08 by interpreter round i1 wave 44, lane
L2, while building
`docs/known-issues/interpreter/i43-L2-proposal-exits-at-a-withdrawn-bodys-constant-pool-helper-calls-20261007.md`.
Wave 45 (lane L2): built for OSR bodies of both tiers, behind
`jit/src/not_entrant.rs::OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE` (on in the
lane's last commit); method-entry bodies remain (see "Progress (wave 45)"
and `docs/internal/fixed-bugs/interpreter-L2-proposal-method-entry-obsolete-bodies-leave-through-the-rebuild-sinks-FIXED-20261010.md`).**

## Progress (wave 45) — lane L2: an own-class OSR body of a renumbered class leaves

**Built.** When a redefinition renumbered the class's constant pool
(`RedefinitionHistory::last_redefinition_moved_constants`, read under the
class-manager writer in `vm_exec.rs::redefine_class_with` and passed through
`JitRealm::note_class_redefinition_renumbering`), the force pass
(`JitCache::force_withdrawn_exit_polls_after`) no longer spares an OSR body
compiled from the class's own bytecode (`compiled_via_osr`, or an optimizing
OSR body: `ir_osr_entries` non-empty): it marks it
(`CompiledMethod::leaves_as_renumbered_obsolete`, before any site is written)
and forces it as it forces a body a redefinition of another class withdrew.
The verdict (`helpers::jit_safepoint_loop_exit_verdict` ->
`PollingBody::with_leaves_as_renumbered_obsolete` ->
`jvmti_events::withdrawn_body_may_leave`, a CROSS-LANE edit) then answers
"leave" for it, and the frame leaves at its next exit of any kind.

**Where this departs from the page's idea, and why.** The page proposed
forcing only the constant-pool helper exit sites and answering "leave" at a
site but "stay" at the back-edge polls. With a "leave" verdict there is no
per-iteration slow path to avoid: the first forced exit reached leaves, and
the rest of the activation is interpreted. Forcing every exit also covers
what the sites alone would not: the optimizing tier (it has no constant-pool
helper sites; see the i43-L2 proposal's "Progress (wave 45)"), a loop whose
constants are read only through a call, and a single-pass loop whose sites
were refused. No verdict needs a site-kind bit, so no emitted code changed.

**Why an OSR body's exit is safe to resume, checked first as the page
asked.** It is not resumed by a stash sink (which is what
`withdrawn_body_may_leave` refused for: they re-ran a frame of a redefined
class from entry) but by the OSR door itself, in place, into its own live
interpreter frame (`jit_bridge::try_osr`: `transfer_osr_exit_into_live_frame`
for a single-pass body, the planless `transfer_osr_guard_exit_into_live_frame`
for an optimizing one; neither reads the class's redefinition state). That
frame is the obsolete activation. The obsolete-frame machinery has already
moved it onto its body translated into the merged pool (the redefinition's
handshake runs `safepoint_check` on every thread that polls, the compiled
poll's slow path included), and the door moves it right after such an exit
(`convert_obsolete_frames_if_redefined` in `try_osr`: an exit through a
forced post-call or constant-pool site passes no `safepoint_check`, and a
renumbering redefinition whose fence was not raised -- the compaction case
of `obsolete_frames::redefinition_moves_constants` -- arms no loop's poll
word); a busy class-manager lock defers that to the loop's next top, before
any bytecode (the deferral bumps the thread's poll word:
`interpreter::loop_top_poll`, `obsolete_frames::retry_deferred_conversion`). The translated body has the
same instruction layout, so the transfer's resume bci and the exit map's
locals and stack carry over. The exit is committed, not charged
(`try_osr`'s `left_for_interpreter` for a withdrawn body's `OsrExit`), and
the frame never enters an OSR body again (`obsolete_frames::frame_runs_replaced_code`).

**Positive controls** (probe
`tools/probes/interp/L2/L2W45OwnClassLoopLeavesAtRenumbering.java`, agent
jar; HotSpot `swap wrong=0 threw=none after-swap-laps=true`,
`old-activation-constant=spinA`, `fresh=tagB`):

* `CRATONVM_DBG_JITC=1`: `[cratonvm-jitc] own-class OSR body leaves (renumbered pool): L2W45OwnClassLoopLeavesAtRenumbering$Tgt.spin:(Ljava/lang/String;)I ...`;
* `CRATONVM_DBG_DEOPT=1`: `[cratonvm-deopt] withdrawn body told to leave: ...$Tgt.spin...`
  and an `OSR-exit TRANSFER ...$Tgt.spin...` line.

On the base none of them names `Tgt.spin` (its `exit polls candidate` line
reads `own-class=true`, and it is spared). Unit test
`not_entrant::tests::an_own_class_osr_body_leaves_after_a_renumbering_redefinition`.
`L2W43CompiledConstantsAcrossRenumbering` (four spinning loops, 201
renumbering redefinitions) exercises the same path under contention: its
loops now leave at the first renumbering and meet the redefinition fence as
interpreter loops at every later one.

**What remains:** a METHOD-ENTRY body of the class
(`docs/internal/fixed-bugs/interpreter-L2-proposal-method-entry-obsolete-bodies-leave-through-the-rebuild-sinks-FIXED-20261010.md`);
the HotSpot comparison in "What HotSpot does" was still not read (no HotSpot
sources locally).

## The problem it removes

A compiled body of class `C` that is running when `C` is redefined keeps
running after it (an OSR loop, a long method-entry body). The redefinition
spares its exits (`JitCache::force_withdrawn_exit_polls`: a body compiled
from the redefined class's own bytecode, `compiled_from_class`, is marked
`exits_spared_as_obsolete` and never forced), and the VM's verdict for it is
"stay" (`withdrawn_body_may_leave`). So every constant it names goes through
a helper that translates the body's constant-pool index into the class's
current pool (`vm/src/jit/helpers.rs::CpSite`, `stale_cp_site_index`), for
the rest of the activation. That translation is where waves 21, 43 and 44
found races (a redefinition landing between the judgement and the pool read:
`interpreter-L2-a-spinning-obsolete-frame-sometimes-throws-internalerror-across-a-renumbering-redefinition-FIXED-20261007.md`,
`interpreter-L2-multianewarray-and-type-check-sites-judge-their-index-before-the-pool-read-FIXED-20261008.md`),
and every site pays it on each execution once any class in the process was
redefined (a thread-local memo per `(holder, stamp)` at best).

## What HotSpot does

Not read for this page (the local `C:\craton\jdk25src` holds the Java
sources only, not HotSpot's): the i24-L6 page's "What HotSpot does" records
that the redefinition's safepoint deoptimizes the frames of marked compiled
methods, which then continue in the interpreter. Whether HotSpot marks the
compiled code of the redefined class's OWN old methods, and so moves such an
activation to the interpreter, must be read in `jvmtiRedefineClasses.cpp`
before building this; JEP 109 requires only that the old activation keeps
running its old bytecode, compiled or not.

## The idea

When a redefinition RENUMBERS `C`'s pool (`redefinition_moves_constants`, or
any history step that translates an index), do not spare `C`'s own bodies'
constant-pool helper exit sites (wave 44's `emit_cp_helper_exit_site`): force
those sites (not the back-edge polls, which would make every loop iteration
pay a slow-path call while the verdict stays "stay"), and answer "leave" for
such a body at a site (`POST_CALL_EXIT_VERDICT_ONLY` with a site-kind bit, or
a verdict that knows the asking body is own-class and renumbered). The frame
then leaves at the instruction, the interpreter's obsolete-frame machinery
(`obsolete_frames::convert_frames`) moves it onto its translated body, and
the rest of the activation runs interpreted against the right constants --
the HotSpot shape.

* The translation (`CpSite`) stays for bodies the verdict does not cover (no
  site at that instruction, a splice, the optimizing tier until it has sites).
* A constant-only redefinition that renumbers nothing keeps today's spare:
  there is nothing to translate.

## What it would cost

A running loop of a renumbered class drops to the interpreter at its next
constant-pool instruction, and may OSR back into a body of the new bytecode
only through the existing guards (`jit_bridge::osr_body_generation_differs_from_frame`
refuses a frame of the old generation). A class redefined in a hot loop
(an agent's retransform every few milliseconds) would run that loop
interpreted until it finishes; today it runs compiled with a translation per
constant.

## What to check first

* Which verdict bit a site can ask without changing the back-edge polls'
  answer for the same body (the tail passes only the compile id today).
* That the obsolete-frame conversion accepts a frame rebuilt at an arbitrary
  bci of a renumbered class (the rebuild sinks restamp with
  `compile_cp_stamp`, `stamp_frame_rebuilt_from_compiled_code`).
* A probe: `tools/probes/interp/L2/L2W43CompiledConstantsAcrossRenumbering.java`
  with a positive control (`post-call exit verdict ... own-class ... verdict=<non-zero>`).
