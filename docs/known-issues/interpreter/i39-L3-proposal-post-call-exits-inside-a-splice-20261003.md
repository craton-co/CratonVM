# Proposal: a post-call exit for a real call made inside a splice, leaving through a two-frame chain

**Status: open (proposal, not implemented) -- filed 2026-10-03 by interpreter
round i1 wave 39, lane L3. The last shape of
`docs/known-issues/interpreter/i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md`
that no post-call exit reaches.**

## Problem

A compiled caller that SPLICED a small callee `C.m` and, inside that splice,
makes a real call (the optimizing tier splices callees without an exception
table: `CountDownLatch.await()` is the i24-L6 page's example) keeps running
the rest of the splice, and the caller's code after it, on the old bytecode
of a class the redefinition replaced while the thread was in that call.
Every post-call exit so far refuses a call inside a splice:

* single-pass: `x64/op_invoke.rs::emit_post_call_exit_site` returns when
  `inline_callee_scopes` is non-empty, and `x64/safepoint.rs::branch_mode_exit_target`
  refuses there too (a bare-bci map inside a splice would be keyed by the
  CALLER's pc: `Compiler::may_file_by_bci`);
* optimizing: `ir_lower.rs::post_call_exit_state` refuses "a call inside a
  splice" (i24-L6, "Progress (wave 28)").

The reason is the frame state. An exit after a call inside a splice must
rebuild TWO interpreter frames: the caller at its invoke of `C.m` (suspended,
to receive `C.m`'s result) and `C.m` at the inner call's successor. The
in-place OSR-exit transfer takes one frame, and the trap sinks' chain
builder refuses exactly this chain:
`deopt_resume::chain_inner_scope_redefined_since_compile` answers true when
an inner scope's class was redefined since the compile, because the chain
would be rebuilt from `C.m`'s CURRENT bytecode while its bci and locals
describe the old one.

## What HotSpot does

The redefinition's `Deoptimization::deoptimize_all_marked` patches the
return address of the compiled frame; on return the deopt blob unpacks the
compiled frame's scopes into one interpreter frame per scope (the caller and
the inlined `C.m`), each at its own bci, running each method's OWN bytecode:
the inlined `C.m` activation began before its class's redefinition, so it
continues as an obsolete method (JEP 109), while the caller continues its
current code.

## Design

1. **The artifact keeps each splice's template.** Beside
   `CompiledMethod::compiled_source` (the outermost method's
   `CachedBytecodeMethod`, round 13), keep the spliced callees' templates the
   compile read, keyed by the scope's `(class id, name, descriptor)` --
   `inlined_methods` names them, `splice_scope_class_ids` carries their ids
   already. Arc clones only; set at the same publish sites as
   `compiled_source` (`deopt_resume::stamp_compiled_source_of`).
2. **A chain map at the inner call's successor.** Both tiers can describe a
   scope's frame state inside a splice for their trap exits (the chain
   `ReconstructedFrame::caller_frames` the trap sinks rebuild); the post-call
   site files an `OsrExit` point whose state is that chain: the inner scope
   at the successor with the result pushed, the outer scope at its invoke of
   `C.m` (`RESUME` after it once the inner frame returns). The single-pass
   key problem (`may_file_by_bci`) is solved by keying the point by the
   site's stub, not by bci: the post-call stub already bakes its own point
   address (`MOV DEOPT_ARG0, point` on the optimizing tier), so no bci table
   lookup is needed.
3. **The chain transfer.** A new transfer (next to
   `transfer_osr_exit_into_live_frame` and
   `transfer_osr_guard_exit_into_live_frame`) pushes the inner frames above
   the live one from the chain, as `deopt_resume::push_inlined_chain` does
   for the trap sinks, building each inner scope's frame from the template
   of piece 1 (not the class's current method), then restamps it with the
   body's `compile_cp_stamp` and converts it
   (`obsolete_frames::stamp_frame_at_rebuilt_from_compiled_code`, wave 37's
   indexed restamp): the inner frame becomes an obsolete frame of `C.m`
   running its own bytecode with its own constants and lines. The outer frame
   is the live frame, current.
4. **`chain_inner_scope_redefined_since_compile` answers from the template.**
   With piece 1, an inner scope of a redefined class is rebuilt from the old
   template instead of refused; the trap sinks gain the same rebuild (their
   refusal re-runs a redefined class's frame today).

## Positive control

`tools/probes/interp/L6/RedefineSpliceAfterTheCallProbe.java` with the
default settings (the optimizing tier splices `CountDownLatch.await()`):
its `parked` row prints `first-after-new=true` only when this exit fires. A
`post-call exit verdict ... named=true withdrawn=true` line
(`CRATONVM_DBG_DEOPT=1`) naming the body, and a new `chain` column on the
`exit polls candidate:` line (`CRATONVM_DBG_JITC=1`) counting the chain
sites, would show it.

## Cost and risk

* Per compiled call inside a splice: the same five-byte `NOP` as every other
  site; the chain point and the templates out of line.
* Risk: medium-high -- a second transfer shape, and inner frames built from
  a template the class no longer holds (the frame's `Frame::adopt_redefined_body`
  path and the redefinition history already carry obsolete bodies; the new
  part is building one from the artifact rather than moving a live frame).
* Owner: lane L3 (redefinition and compiled-code retirement) with lane L2
  for the two tiers' map emission.
