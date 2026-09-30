# Proposal: drop the post-swap conversion handshake of a pool-renumbering redefinition

**Status: proposal — filed 2026-10-02 by interpreter round i1 wave 38, lane L3
(review of the wave-37 fence's costs). Not implemented.**

## What is paid now

A redefinition whose pool moves a constant (an IDE HotSwap: `javac` output of
an edited class) takes up to three pauses of every mutator:

1. the fence's handshake before the swap (`obsolete_frames::raise_fence_and_handshake`,
   from `RedefinitionFence::raise` or, for a call of 2+ classes,
   `raise_install_fence` -- since wave 38 only when a class moves a constant);
2. `obsolete_frames::after_redefinition`'s handshake after the swap, once per
   such class -- inside a call-wide fence too, so a HotSwap of N recompiled
   classes takes N of them;
3. the loop-exit handshake (`jvmti_events::request_withdrawn_body_exits`), once
   per call, skipped when (2) saw every peer poll.

## Why (2) no longer does what it was added for

Wave 19 added (2) so that every running thread passes `safepoint_check`, which
moves its frames onto their translated bodies, before it resolves another
constant of a replaced body. Since wave 37 that is covered without it:

* a thread whose loop the fence held converts at its next top once the fence
  is down (`retry_deferred_conversion`);
* every fence drop -- raised or not -- arms every loop word
  (`GcBarrier::lower_redefinition_fence`, `note_code_moved_on_every_loop`), so
  every running interpreter loop converts at its next top
  (`convert_at_loop_top`), before its next bytecode;
* an interpreter frame under a compiled activation runs no bytecode until
  control returns to its loop, whose word is armed;
* a blocked thread converts when it leaves its blocking region
  (`check_post_block_gc`), a thawed continuation at its remount
  (`convert_thawed_frames`);
* compiled code translates its own constant-pool sites through its compile
  stamp (`jit::helpers::stale_cp_site_index`).

What (2) still does: it is the "every peer polled" fact that lets the
redefinition skip (3). Without it, (3) is taken whenever a body was withdrawn,
which is the same one pause.

## Proposal

`after_redefinition` keeps its own-thread conversion
(`convert_obsolete_frames_if_redefined` on the redefining thread) and takes its
handshake only when this redefinition withdrew a compiled body and no batch is
open (then it IS (3), and `request_withdrawn_body_exits` is skipped as now);
otherwise no pause. Inside a call-wide fence, none at all: the call's end
(`end_redefinition_batch`) takes (3) once.

## How to verify

* `L3W37HotSwapSpinningReads`, `L3W37RedefineTwoClassesAtOnce`,
  `L3ObsoleteParkedAcrossToggles` / `...Renames` / `...VirtualThreadResume` and
  the obsolete-frame repeats (`rep.sh`), JIT and `--nojit`: unchanged.
* Pause count: `CRATONVM_DBG_RETRANSFORM=1` plus the non-collection pause
  census, on a HotSwap of a 10-class call: 1 + 10 + 0 pauses before, 1 (+1 when
  a body was withdrawn) after.

## Risk

A path that relies on a peer having converted by the time the redefinition
returns (not found: every reader of a frame's code is the frame's own thread,
which converts before its next bytecode). The census floor
(`publish_stale_frame_floor`) is published at each thread's own conversion,
handshake or not, so history pruning is unaffected.
