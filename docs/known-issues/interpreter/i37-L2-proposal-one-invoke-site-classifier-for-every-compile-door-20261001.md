# Proposal: one invoke-site classifier for every compile door

**Status: proposal — filed 2026-10-01 by interpreter round i1 wave 37, lane L2.**

## Problem, with evidence

Four places decide how a compiled `invoke*` site binds, and each spells the
decision out itself:

| Where | Private `invokevirtual` / `invokeinterface` pin | `final` devirtualisation | JVMS §6.5 `invokespecial` owner | Owner id baked (`JitInvokeInfo::owner_class_id`) |
|---|---|---|---|---|
| `CpResolvers::invokespecial_owner` (`jit_bridge.rs`; method entry, callee doors, optimizing OSR) | yes | yes (`invokevirtual_site_final_owner`) | yes (`select_special` / `invokespecial_selection_start`) | yes |
| `compile_osr_body`'s invoke loop (`jit_bridge.rs`, single-pass OSR) | yes (`invokevirtual_site_targets_private`) | no | **no** — binds the constant-pool class | no (`push_osr_invoke_info` has no owner) |
| the eager first-call door (`interpreter.rs`, `CRATONVM_BG_COMPILE=0`) | yes | no | **no** | no (`owner_class_id: 0`) |
| `resolve_inline_site_from`'s callee-body scan (every splice) | yes, kinds 0/2 | no | **no** — kind 1 at the constant-pool class | for the private pin only |

Wave 37 (lane L2) found the third column's gap while closing the last half of
`interpreter-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot-FIXED-20261001.md`:
a super call that names an ancestor above an override, or whose selection is
an error, was bound by the constant-pool class in three of the four places.
The fix (`special_site_bind_refusal`) had to be added to each of them
separately, and it REFUSES at the three that cannot substitute an owner
(the OSR loop is denied, the eager door seals, the splice is refused),
because they have nowhere to put one. Every earlier JVMS-selection fix in
this round landed the same way, one door at a time (the private pin arrived
in each door by a separate defect: vert.x `TCPSSLOptions.init()` for the
eager door, the OSR door's own comment, `cha-binds-a-private-interface-method-
to-the-receivers-public-namesake` for splices).

## Design

1. Move the decision into one function over the caller's class-manager
   guard, e.g. `jit_bridge::classify_invoke_site(shared, cm, holder, cp_idx,
   opcode) -> SiteBinding`, returning the kind (virtual / statically bound /
   interface / static), the owner name AND id to bind, and a refusal reason
   (the §6.5 error, a §5.4.4 denial, a recorded resolution failure). It is
   `CpResolvers::invoke` + `invokespecial_owner` + `special_site_bind_refusal`
   as they are now.
2. `CpResolvers` answers its two resolvers from it; `compile_osr_body`, the
   eager door and `resolve_inline_site_from` call it instead of their own
   loops' classification, and carry the owner id into `push_osr_invoke_info`
   / `JitInvokeInfo` / `InlineInvokeTarget` (each already has, or can take,
   the field).
3. Then the three refusals wave 37 added become substitutions: a
   grandparent-named super call binds its selection start at every door
   instead of costing an OSR denial or a splice.

## Expected win and how to measure it

Correctness by construction: a selection fix lands once. Performance: the
`invokespecial-selection-start` refusals wave 37 added (an OSR loop denied,
a splice refused) become binds; count them first with `CRATONVM_DBG_JITC=1`
(`invokespecial-selection REFUSED osr ...` / `inline-resolve REFUSED ...
invokespecial-selection-start`) on the suite and Spring Boot. If both counts
are zero there, the performance half is worth nothing and only the
maintenance half remains.

## Risks

The OSR loop and the eager door hold `cm_lock` across their whole loop and
feed several side tables (`pending_ctor_sites`, `pending_callee_compiles`,
the String intrinsics); the classifier must not take a lock of its own and
must leave those tables' order alone. `--compatible` must see no change
beyond the §6.5 owner (which is its own interpreter's answer).
