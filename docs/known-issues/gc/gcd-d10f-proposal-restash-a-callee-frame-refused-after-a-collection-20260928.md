# Proposal: pin a taken callee stash across its rebuild, so a refusal after a collection restashes instead of forcing a re-run

> **STATUS (2026-09-28, gcd d10/f, lane frames10): PROPOSAL.** Nothing is
> built. **Size:** S-M. Common to every collector (the rebuild's allocations
> are what can collect; the frame's words are what a moving young or
> relocating old cycle rewrites).

*Filed 2026-09-28 by gcd wave d10, lane f.*

## Why

`vm/src/jit/helpers.rs` `try_resume_trapped_callee` TAKES the stashed frame
of a trapped compiled callee (`take_last_deopt_with_point`) and rebuilds an
interpreter frame from it (`build_deopt_frame_or_refusal`), which can
allocate (the virtual-object materialiser, the pool refill) and so collect.
Out of the stash, the frame's `Object` words are rooted by nothing. When the
build is then refused, the frame cannot go back into the stash, because its
words may name from-space; the "refused after a collection" arm DROPS it and
raises the frameless deopt flag, and the call site answers with a re-run:
of the callee from entry (`service_frameless_callee_trap`: a replay of the
callee's prefix), or, at a d9/d / d10/f owned-arguments site, of the CALLER
(`gcd-d10f-owned-args-handoff-replays-the-caller-unchecked-20260928.md`).
The refusals that reach this arm are allocation failures after shells were
allocated -- heap exhaustion, which is exactly where the OOME ladders of
this round live.

## The proposal

1. Before the build, push every `Object` word of the taken frame
   (locals, stack, monitor objects; `rframe.locals` / `stack` /
   `monitors`) onto `native_pin_roots`, above the compiled-locks pins, in a
   fixed order (the `pin_object_args` / `refresh_args_from_pins` pattern of
   `exception_dispatch.rs`).
2. On a refusal after a collection, write the pinned addresses back into
   the frame (the same order), release the pins newest-first
   (`scripts/pin-stack-order-audit.py`), and restash it
   (`restash_last_deopt_with_point`), as the no-collection refusal already
   does. The next sink then sees an ordinary unmappable frame with current
   addresses, charged and handled as today.
3. Better still for the heap-exhaustion case: classify the refusal
   (`frame_build_heap_failure_is_answered`) and answer `OutOfMemoryError` at
   the call, as the sibling arm does for a materialisation that exhausts the
   heap -- HotSpot throws OOME when it cannot rematerialise.
4. Count both outcomes in the door census
   (`deopt_resume::door_rerun_census`), so a workload can show the GC-drop
   re-runs went to zero.

## How to verify

`GenR4W6JitOomRootProbe` and `GenR4W4HeapFullThrashProbe` unchanged (their
HotSpot lines, 3/3 on an idle host); the door census printing zero
`callee-frameless` re-runs caused by the GC-drop arm under
`CRATONVM_JIT_THRESHOLD=1` on the OOME battery; a unit test that forces a
collection inside a refused build (`build_deopt_frame_or_refusal`'s
`stress` parameter) and asserts the restashed frame's object words equal the
pins' current addresses. On each of `-XX:+UseGenerationalGC`,
`-XX:+UseG1GC`, `-XX:+UseZGC`.
