# C14W3-2 (IC hit arms' cold tails): the taken branch cannot go without an out-of-line polymorphic region

Status: OPEN (design; nothing landed -- the contained variants buy nothing, see below)
Area: JIT optimizing tier lowering (`jit/src/ir_lower.rs` `emit_inline_cache_call`, `emit_ic_hit_exit`, `emit_deferred_self_call_tails`)
Severity: LOW (performance: one taken branch and one extra i-cache span per monomorphic IR virtual call)
Found by: round 14 wave 4 lane calls2

## What the proposal wanted

C14W3-2 (`jit-r14-calls3-proposals.md`): CC3-1's other half. A monomorphic IR virtual call's MIC
hit leaves through `emit_ic_hit_exit`: `CMP RAX,1; JNO .hit_join` TAKEN on every hit, jumping over
the inline callee-deopt service, the four PIC rungs, the megamorphic staging and hashed stub, the
resolving helper, `.done` and `.tail` (several hundred bytes) to `.hit_join` (shadow reload, result
store). The census the wave-3 calls lane added (`ir-call-cold-tails ... ic_hit_exits=<n>
ic_admissible=<m>`, `ir_lower.rs` `emit_ic_hit_exit`, printed under `CRATONVM_DBG_JITC=1`) counts
how many exits the CC2-1 admission would take.

## Why the contained variants were not landed

Every variant that keeps the polymorphic region INSIDE the site's code keeps one taken branch on
the MIC hit, so none of them is worth its risk:

1. **Defer only the service** (the `CrossCallSentinel` shape): the hit becomes `CMP; JO .cold`
   (not taken) then `JMP .hit_join` (taken) -- the same one taken branch, one more instruction.
2. **Emit `.hit_join` right after the MIC arm** (reload + store, then fall into the rest): the hit
   falls through the join but must then `JMP` over the PIC rungs, stub, helper, `.done` and `.tail`
   to the next node's code -- again one taken branch; the rungs, the stub and `.done` jump BACK to
   the join instead. Byte count and branch count on the hit are unchanged.
3. **Put the polymorphic region ahead of the MIC guard**: the straight-line code before the site
   has to jump over it -- a taken branch at site entry instead of at exit.

The branch disappears only if the polymorphic region (PIC rungs, megamorphic staging, hashed stub,
resolving helper, `.done`, `.tail`) is emitted OUT OF LINE, after the body, with the MIC hit
falling through `.hit_join` into the next node. That region is not a `DeferredSelfCallTail`-shaped
blob: its emission reads lowering state that exists only while the call node is being lowered.

## What an out-of-line region has to carry (the exact plan)

Read from `emit_inline_cache_call` (~15000-15460 at `20a668e12`):

| emitted in the region | state it reads at the site | how the deferred form gets it |
|---|---|---|
| PIC rungs: `emit_ic_shift_for_context`, `emit_call_loaded_ic_entry` | the premarshalled ABI registers (still live: nothing between the MIC miss `JNE` and the region touches them if the region is entered straight from it) | nothing to capture; the region must be entered by the MIC miss `JNE` itself, never through other code |
| each rung's `emit_ic_hit_exit` service (`emit_callee_deopt_service_cold`, `after_reload = false`) | `gp_load_value` of each argument, `pending_shadow_copy_back()`, `emit_post_call_frame_record` | `SelfCallArgSource` per argument (as CC2-1 / CC3-1 capture) -- and the CC2-1 admission (`body_publishes_no_roots`, no pending shadow copy-back), because a copy-back needs the shadow offsets |
| megamorphic staging (`gp_load_value(R11/RAX, arg)`), `arg_offsets` (`slot_of`, `home_dropped`) | register residency and home slots | captured sources, as above; `arg_offsets` are plain frame offsets (capturable) |
| hashed stub (`emit_hashed_vtable_stub_reloading[_with_gate_entry]`) | `pending_shadow_copy_back()` (`ShadowCopyBack`) | refuse the deferral when a copy-back is pending (the admission above) |
| `.done`: `note_inline_frame_return_site()` | the current node's inline chain, recorded at `buf.pos()` | the chain is keyed by the node; capture the chain key at the site and record it at the deferred offset (every other return site is recorded where it is emitted, so the map stays exact) |
| `.tail`: `emit_call_return_sentinel_tail(ty)` -> `push_call_exc_patch` | `cur_bci`, `bci_is_protected`, `site_snapshot_may_hold_monitor`, a rethrow pad's frame state (`resolve_frame_state_for_site`, from the node being lowered) | the CC3-1 split: `note_deferred_cross_call_exit` at the site, the plain row pushed at emission; refuse at a protected bci (no reason-9 pad from a deferred region) |
| `.hit_join`: `emit_shadow_reload()` | the site's shadow publication | stays INLINE: it is the fall-through of the MIC hit and the region's return target (`emit_jmp_back_to`) |
| `emit_ic_stack_sample()` | none after the site | stays inline, ahead of the guard |

So the shape is: at the site, emit the guard and MIC arm, then `CMP RAX,1; JO .mic_cold` (not
taken), then `.hit_join` (reload, store) and continue with the next node; record a new
`DeferredSelfCallTail::IcRegion { mic_miss_patch, mic_cold_patch, hit_join, info_ptr, mic, pic,
num_args, args: Vec<SelfCallArgSource>, arg_offsets: Vec<i32>, ty, bci, inline_chain_key,
gate_entry, wide_block }` and let `emit_deferred_self_call_tails` emit the rungs, the stub, the
helper, `.done` / `.tail` there, every exit jumping back to `hit_join` (a `.tail` exception exit
takes `call_exc_patches` as CC3-1's does). Admission: `mir_mode == Off`, unprotected bci,
`body_publishes_no_roots()`, no pending shadow copy-back, not an over-wide site (`wide_block`,
whose MIC arm falls into a marshalling tail the rungs jump back to), the republish switch on
(`hits_republished`), and a new kill switch (`CRATONVM_JIT_IR_IC_COLD_REGION`, default OFF until
the census and a probe say it pays).

What makes it more than a medium change, and why it was not attempted blind this wave: every table
that records a BUFFER OFFSET while the region is emitted today -- the inline frame return site, the
call-exception patch list, the region's own internal rel32s -- must be recorded at the deferred
offsets instead, and the `ir_lower` unit tests that walk an IC site's bytes (for example
`r13w5_screened_megamorphic_edges_enter_the_stub_gate_entry`) read today's order. It needs a
compile-and-run loop, not a lane that cannot build.

## How to decide it

1. Census first: `CRATONVM_DBG_JITC=1` on `hashmap`, `R12Mega4OneSite` and CratonBench; sum
   `ic_hit_exits` and `ic_admissible`. Worth it only where `ic_admissible` is most of `ic_hit_exits`
   and the IC sites are monomorphic (`CRATONVM_DBG=mic-prof`: `mic_hits` dominant).
2. An upper bound without the work: the `mono` phase of `R12Mega4OneSite` against `static24`
   (the same loop with a direct call, which already has CC3-1's fall-through) bounds what the
   MIC hit's taken branch and its hot-span size cost.
