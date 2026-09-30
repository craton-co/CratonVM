# Proposal: the dispatch helpers check, on return, whether their compiled caller was withdrawn during the call

**Status: open (proposal, not implemented) — filed 2026-10-05 by interpreter
round i1 wave 41, lane L2, from the reading behind the compiled half of the
install fence
(`interpreter-L2-serving-a-redefined-target-compiled-tears-a-multi-class-redefinition-FIXED-20261006.md`,
"Progress (wave 41)").**

## Problem

A compiled frame that is inside a call when a redefinition withdraws its
body returns into the old body and runs on until a post-call exit at that
site, or the next exit-capable back edge. Post-call exits are emitted site by
site, by both tiers, and five shapes have none
(`i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice`,
"Progress (wave 40)"), nor does a call inside a splice
(`i39-L3-proposal-post-call-exits-inside-a-splice`). The install fence of a
multi-class redefinition holds the interpreter loop a compiled caller calls
into until the last class is installed, so a call from compiled code into a
class of the call spans the other installs by construction; wave 41 withdraws
dependents ahead of the first install to keep such callers out of the
window, but a caller already past its poll still makes the call.

Many such calls do not return through a site's own code at all: a call the
single-pass or optimizing tier emits to `jit_invoke_dispatch` /
`jit_invoke_virtual_mic` returns through the helper, which is Rust and knows
the call has ended.

## Design

1. **The helper learns its caller's body once, on the slow side.** The
   not-entrant stub already reads its caller's return address
   (`NotEntrantRecord::caller_return_address`,
   `conservative_roots::swap_top_not_entrant_ret_pc`); the dispatch helpers
   get the same word from their frame. The body is found from it only when
   it is needed (step 2), through the code cache's address lookup
   (the one the stack walks use to name a compiled frame by pc).
2. **A cheap trigger.** Snapshot `JitCache::bodies_withdrawn_by_redefinition`
   (one load) at helper entry; on return, compare it again. Equal: return as
   now. Different (a redefinition withdrew something while the callee ran):
   find the caller's body (step 1) and ask
   `JitCache::body_withdrawn_by_redefinition`.
3. **Leave on return.** A withdrawn caller is sent to the interpreter at the
   call's successor with the callee's result, which is exactly the post-call
   exit's transfer; where the site has an exit point, the helper arms it
   (the forced post-call patch already is), and where it has none, the
   helper returns the deopt sentinel with a pending "resume after the call
   with this value" record for the caller's own drain, the frame state the
   site's re-execution map gives, without re-running the callee (the
   `frameless_rerun_is_exact` family is the precedent for when a re-run is
   exact; this must never need one).

## Positive control

`L2W41TwoClassRedefinitionReadTrace` with a caller that splices one class of
the call and calls the other through a helper (a site with no post-call exit
by the i24-L6 list): `stale-q` and `torn` fall to 0, and a new
`[cratonvm-jitc] helper return: caller withdrawn during the call, left at
bci N` line names the site.

## Cost and risk

* Per helper call: one load at entry and one compare at return (the helpers
  already do far more); nothing on raw calls, inline-cache hits or direct
  binds, which this does not cover (their sites need the post-call exits).
* Risk: medium. Step 3's no-exit-point arm is a new transfer shape. Step 1
  depends on an address-to-body lookup being available on the mutator
  without a lock the redefinition holds.
* Owner: lane L2 (the helpers), with lane L3 for the transfer.
