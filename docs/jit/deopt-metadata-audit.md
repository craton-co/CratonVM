# Deopt metadata soundness audit

Scope: *can the emitted metadata reconstruct the interpreter state **exactly**?*
Companion to [`deopt-metadata.md`](deopt-metadata.md), which records what is
emitted and by whom — this one records, per recorded state, whether the mapping
to interpreter state is **total**, and what was changed. It does not restate the
completeness table; read that one first.

Files audited: `jit/src/deopt.rs`, `jit/src/ir_verify.rs`. Producers
(`jit/src/ir_lower.rs`, `jit/src/x64.rs`) and consumers
(`vm/src/runtime/interpreter.rs`) were read but not edited.

The standing rule this audit applies: **if the metadata cannot describe a state
exactly, refuse.** A deopt that reconstructs a *plausible* frame instead of the
*correct* one produces a running program with silently wrong values, which is
strictly worse than a bailout — a bailout only costs the optimizing tier.

---

## 1. Audit table

| Recorded state | What the interpreter needs | Total? | Action |
| --- | --- | --- | --- |
| **Locals — presence** | exactly `max_locals` slots, dead ones explicitly nothing | **no** | Not closed. `resolve_frame_state_for_bci` (`ir_lower.rs:4168`) falls back to `locals: Vec::new()` when a bci has no snapshot, and the install site registers `max_locals: u16::MAX` (`ir_lower.rs:6893`), so the count lane is inert on the IR path. See §6.1 |
| **Locals / stack — type** | per-slot JVM category (`int`/`long`/`float`/`double`/`ref`) | **yes, per descriptor**; **no, across descriptors** | Closed for the *internal* contradiction: one frame word described with two categories is now `SlotTypeConflict`. Closed for the *external* one: a word the oop map calls a reference and the deopt map calls a primitive is now `PrimitiveSlotCoveredByOopMap` |
| **Locals / stack — location** | a word inside the trapping frame | **was not checked at all** | Closed: `FrameSlotOutsideFrame` rejects any `StackSlot*(off)` with `off >= 0` |
| **Register descriptors** | index inside the 16-entry file the stub spills | yes | Unchanged (`check_gpr_index`, and `try_resolve_value` degrades to `Unsupported` at runtime) |
| **Recorded oops — GC visibility** | the collector must see and *remap* the word | yes, when the map claims completeness | Unchanged forward lane; converse direction added. See §2 |
| **Baked object addresses** | nothing can update them | yes | Unchanged (`BakedObjectAddress`) |
| **Eliminated values** | refuse, do not resume as `Int(0)` | yes for `MaterializationRequired`; **no** for a producer that still writes `Undefined` | Not closed here — it is a producer change (`deopt-metadata.md` §2/§5.7). Undetectable from the metadata by construction |
| **Virtual objects** | one definition per id, `num_fields == field_values.len()`, every ref resolves | yes, per scope | Unchanged |
| **Inlined frame chain** | every frame, each with its own method and bci | **no producer builds one**; the consumer side is now fail-closed at the cap | Closed the truncation holes: `intern_with`, `materialize`, `chain_is_resumable`, `check_point`. See §3 |
| **Exception state** | throwing bci + post-pop stack, routed through the exception table, never resumed | routing is keyed on `reason`, semantics on `semantics` — they could disagree | Closed: `ResumeSemanticsMismatch` enforces `reason == PendingException ⟺ semantics.rethrow_exception` |
| **Monitors — shape** | a lockable object per entry, depth ≥ 1, no duplicate | **was not checked**: an `int` monitor passed | Closed: `MonitorObjectNotAReference` |
| **Monitors — presence** | one entry per lock the frame holds | **NO — this is the open hole.** The IR path records `monitors: Vec::new()` unconditionally while compiling `synchronized` blocks | Not fixable in these files. See §4 — this is the one cross-file change requested |
| **Point ordering** | `find_deopt_point`'s binary search | yes | Unchanged |
| **Snapshot identity (IR)** | one snapshot per bci | **was not checked** | Closed in `ir_verify::check_frame_states`. See §5 |

---

## 2. The GC-root argument for recorded oops

Two halves, and both have to hold.

**Half one — the collector must *see* the word.** This is the rule
[`deopt-metadata.md`](deopt-metadata.md) §3 states: a `StackSlotRef(off)` the
deopt map reads as an object must appear in the oop map as the positive
`[rbp - off]`. Enforced by `ReferenceNotInOopMap` /
`ReferenceRegisterNotInOopMap`, gated on `moving_young_coverage_complete`,
unchanged by this audit.

**Half two — the collector must be able to *write back* through it.** Naming the
slot is not enough: `scan_oop_slots` yields `ObjectRef` values that mark but
cannot be updated in place, so `emit_safepoint_map` (`ir_lower.rs:1086`) only
claims `moving_young_coverage_complete` when `coverable && (published ||
slots.is_empty())` — `published` being the shadow-stack push that gives the
collector writable roots. So the completeness flag the verifier keys on already
means *both* halves; a map that could not publish claims `false`, which diverts
the cycle to the non-moving sweep, and an uncovered reference in that frame
cannot go stale because nothing moves. That is why the lane is gated on the flag
rather than on the presence of a map.

**What this audit added: the converse direction.** §3 of the older doc says an
oop-map slot the deopt map does not name is *not* an error — true, and it stays
true (`an_oop_map_slot_the_deopt_map_never_names_is_not_an_error`). But a word
the deopt map **does** name, as an `int`/`long`/`float`/`double`, that the oop
map lists as holding a live reference, is a flat contradiction: one machine word
at one safepoint holds one value. Either the resume hands the interpreter a
truncated `Int` where a live reference belongs — which also drops the oop from
the resumed frame's root set — or the collector relocates against a word that is
not a pointer. `PrimitiveSlotCoveredByOopMap` (`deopt.rs:4272`) rejects it.

This is sound on the IR backend *by construction*: `plan_slots` keeps disjoint
`Ref` and `Prim` free lists, so "a colour is `Ref` or `Prim` from its first
assignment and never changes class" (`ir_lower.rs`, *Reference / primitive
separation*, with its own test `a_reference_is_never_aliased_with_a_non_reference`).
The lane is therefore a tripwire on that invariant there, and a real check for
`jit/src/x64.rs`, whose deopt map (`local_oop_masks` / `stack_oop_marks`) and oop
map (the register allocator's live sets) are built from **different sources** and
have never been compared — which is the gap the verifier exists to close, and
whose install site is still unwired (`deopt-metadata.md` §6).

Related producer note, found while establishing the above and **not fixed**
(other agent's file): `x64::frame_value_for_slot` (`x64.rs:1197-1213`) returns
`FrameValue::Register(r)` for a register-resident slot *regardless of `is_oop`*,
so a register-homed reference is recorded as a cat-1 `int`. `FrameValue::RegisterRef`
exists for exactly this. The new lane would catch it the moment the x64 install
site is wired and its maps register GPRs.

---

## 3. The monitor-balance argument

A `MonitorInfo` list is replayed as one `monitorenter` per entry on resume, and
the interpreter's method-exit path emits one `monitorexit` per entry it believes
the frame holds. The list is therefore not descriptive — it is the *count* that
balances the unwind. Three ways it can be wrong, and where each stands:

1. **`lock_depth == 0`** — a lock nobody holds. Rejected before this audit.
2. **The same object as two entries** — re-entrancy must be one entry with
   `lock_depth > 1`; two entries enter it twice and leave the interpreter one
   `monitorexit` short. Rejected before this audit (by `FrameValue` equality, so
   it catches two identical descriptors, not two different descriptors of the
   same object — an alias the metadata cannot express).
3. **An entry naming something that is not an object** — *was not checked*. A
   `MonitorInfo { object: FrameValue::Int(5) }` passed verification: `Int` is not
   `Unsupported`, not `MaterializationRequired`, not `Undefined`, and the
   catch-all arm of `check_value` ignored it. The resume would build a
   `Value::Int`, the exit would try to unlock it, and the real monitor would stay
   held — a hang in whatever thread asks for it next, arbitrarily far from the
   deopt that caused it. `MonitorObjectNotAReference` (`deopt.rs:4334`) now
   rejects every primitive descriptor and an explicit `Object(0)` (`monitorenter`
   on null throws rather than locking, so a null here means the emitter lost the
   object).

Monitor objects also go through the full oop-map agreement lane — a lock the
collector cannot see is re-acquired on a stale address after a relocating young
collection (`a_monitor_object_is_checked_against_the_oop_map`).

### 3.1 The hole none of that closes

**`jit/src/ir_lower.rs` compiles `synchronized` blocks and records no monitors.**
All three `FrameState` construction sites hard-code `monitors: Vec::new()`
(`ir_lower.rs:4176`, `:4475`, `:4603`).

`deopt-metadata.md` §5 item 4 records this as coverage loss "masked by
`can_deopt_resume = false` for any method with an elided monitor and by the VM
sink's `is_synchronized` bail". **That masking argument no longer holds**, and
the premise should be corrected:

* `IrBuilder` **does** have a `monitorenter`/`monitorexit` arm — `ir.rs:5058`
  (`0xc2 | 0xc3`), which the older comment at `ir_lower.rs:6605` still describes
  as absent;
* `ir_lower` **does** lower them, to real helper calls (`ir_lower.rs:3088`);
* the refusal guard at `ir_lower.rs:6614` only fires when
  `helpers.monitor_enter == 0`, and the production table wires it
  (`vm/src/jit/helpers.rs:13064`), so the guard is inert in a real VM;
* the VM sink's bails are "the frame holds monitors" and "the method is
  `ACC_SYNCHRONIZED`" (`vm/src/runtime/interpreter.rs:13754`). A `synchronized`
  **block** in a non-synchronized method trips neither — precisely *because* the
  metadata recorded zero monitors. The bail cannot fire on information the
  metadata omitted.

So a deopt inside a `synchronized` block resumes a frame that believes it holds
no locks, and the monitor is never released. What actually contains this today is
the default-off `CRATONVM_DEOPT_REAL` gate on the whole precise-resume path: with
it off the deopt takes the whole-method re-run, which re-executes the
`monitorenter` on a re-entrant lock and stays balanced. That is a feature flag,
not a correctness argument, and it is exactly the kind of prerequisite the
speculative-optimization lanes are being built on top of.

**Cross-file change needed (not made — `ir_lower.rs` is another agent's file).**
In `lower_inner_with_scopes`, next to the existing monitor-helper guard at
`ir_lower.rs:6614`, refuse any graph carrying monitor ops while
`resolve_frame_state` cannot describe them:

```rust
// A monitor open at a deopt bci is not recorded (`resolve_frame_state`
// hard-codes `monitors: Vec::new()`), so a precise resume would rebuild a
// frame holding no locks and never release the real one. Refuse until the
// snapshot carries the monitor stack.
if graph.nodes.iter().any(|n| matches!(n.op, Op::MonitorEnter | Op::MonitorExit)) {
    return refuse(Bailout::with_context(
        BailoutReason::UnsupportedShape("monitor open at a deopt point"),
        "graph contains monitor ops but safepoint snapshots record no monitor \
         stack; a resume would leave the lock held",
    ));
}
```

The cheaper alternative — clearing `can_deopt_resume` for such a method instead
of refusing the compile — is also sound and keeps the optimized code; it is the
same trade the x64 backend already makes. Either is better than the current
state. The `deopt-metadata.md` §5 item 4 masking claim should be corrected
alongside it.

---

## 4. Inlined scope chains: three silent truncations, closed

No producer builds a chain (`deopt-metadata.md` §5 item 2), so none of this is
live yet — which is the reason to fix it now rather than after a producer lands.
Every walk in `deopt.rs` is bounded by `MAX_SCOPE_CHAIN` (256). Three of them
were bounded by **stopping**, which returns a well-formed answer about a stack
that is not the one that trapped:

| Site | Was | Now |
| --- | --- | --- |
| `FrameStateInterner::intern_with` (`deopt.rs:2561`) | cut the chain at the cap; the last kept scope's `caller` became `None`, so the interned state *claimed* to be a complete stack | appends an explicitly unreconstructable terminator scope (`FrameValue::Unsupported`, `bci: u32::MAX`, empty key) so the cut is visible to every consumer that already refuses one |
| `FrameStateInterner::materialize` (`deopt.rs:3011`) | `break` — returned `Some(truncated_chain)` | returns `None` when frames remain above the cap |
| `FrameStateInterner::chain_is_resumable` (`deopt.rs:3107`) | `break` — returned `true` for a chain it had not finished walking | returns `self.caller(handle).is_none()`, matching its owned twin `frame_state_is_resumable` (`deopt.rs:417`), which has always refused there |
| `DeoptVerifier::check_point` (`deopt.rs:3825`) | unbounded walk, no finding | bounded, and the cap is a finding (`ScopeChainTooDeep`) |

`chain_is_resumable` mattered most: its two callers are **compile-time admission
gates** (`x64.rs`'s unresumable-`invokedynamic`-trap bail and
`CompiledMethod::osr_exit_policy`) deciding *"may this artifact ever be
entered"*. Answering `true` on the strength of scopes nobody looked at is a
fail-open in an admission gate. Its own doc already said it was "the handle-side
equivalent" of the owned predicate; the two disagreed at exactly one input, and
`the_two_resumability_predicates_agree_at_the_cap` now pins them together.

`violations_interned` (`deopt.rs:4369`) distinguishes the two reasons
`materialize` can refuse — an unissued handle (nothing is readable) vs. a chain
past the cap (readable but undescribable) — because reporting the first for the
second sends a reader looking for the wrong bug.

---

## 5. `ir_verify.rs` persistence audit

The named prior defect — a stub graph from an abandoned build setting a sticky
bail that dropped a later *successful* build out of the optimizing tier — is
fixed and the fix is where it should be: the `unbuilt` early return at
`ir_verify.rs:410-414`, which returns `Ok(())` for a graph the front end never
finished. The accumulator that carried the poison, `ir_verify_bail`, is a
compile-local `let mut` (`lib.rs:12974`), not a static.

Swept for more of that shape; **nothing else of it is present**:

* **All persistent state** in the module is two `OnceLock<bool>` config latches:
  `verify_enabled()` (`:314`) and `pre_lower_verify_disabled()` (`:328`). Both
  latch an environment read on first use. They are process-global and therefore
  *do* survive across compilations and across VMs in one process — a second VM
  in the same process inherits the first VM's setting even if its flags differ.
  That is a known standing property of declared flags here, not a verifier bug,
  and it cannot poison a *verdict*: the latch selects which lanes run, never
  whether a particular graph passed.
* **No caches.** `Violations` is constructed fresh per `verify_graph` call and
  consumed by `into_result`. No per-node, per-graph, or per-method memo exists,
  so there is nothing keyed on something that is not unique to one build.
* **No mutation.** Every lane takes `&Graph`. `verify_graph`'s contract ("never
  panics, never mutates") is structurally true.

**Reachability audit** — which checks actually run:

| Lane | Reached when | Note |
| --- | --- | --- |
| edges / arity / phis / control | always | |
| types | env `CRATONVM_JIT_VERIFY_TYPES` only | never on by phase |
| frame states | `PHASE_POST_OPTIMIZE`, or env | production hook is gated by `verify_enabled()`, i.e. debug builds or `CRATONVM_JIT_VERIFY_IR=1` |
| memory chain | same | |
| arena order | **env only — `for_phase` never enables it at any phase** | deliberate: GVN violates it by construction. Documented and tested (`the_two_ea_constants_gate_different_lanes`) |

One check *is* effectively unreachable and is left alone deliberately: the
`sp.locals.len() > MAX_JVM_FRAME_SLOTS` bound (`:1113`). `max_locals` and
`max_stack` are `u16` in the class file, so a snapshot built from a real method
can never exceed it; the check only guards hand-built and fuzz graphs, which is a
legitimate role for it. It is not counted as coverage of anything.

**Added lane — duplicate snapshot bcis.** Both consumers key on the bci and break
ties by *position*: `ir_lower::resolve_frame_state_for_bci` takes
`position(|s| s.bci == bci)` (first match wins, and that index is also what binds
the frame to its inlined scope through `InlineScopeTable::snapshot_scope`), and
`build_deopt_points` maps every snapshot through `bci_native[sp.bci]` so
duplicates collide on one native offset and `dedup_by_key` drops all but one.
Either way one frame state is silently discarded and the other is used at a
program point it does not describe. The front end's linear bytecode walk visits
each `pc` once, so this holds today — the check is what keeps it holding, and it
is the only invariant in this file whose violation is invisible at the point of
the defect.

---

## 6. What remains unvalidated

1. **Completeness of the locals array.** The verifier checks
   `locals.len() <= max_locals`, never `==`. A frame that simply forgot a local
   passes, and `resolve_frame_state_for_bci` produces a wholly empty frame when a
   bci has no snapshot. Closing it needs the install site to register **real**
   `MethodFrameLimits` instead of the saturated `u32::MAX / u16::MAX / u16::MAX`
   it registers today (`ir_lower.rs:6893`) — the lowerer does not receive the
   method's `code_len` or `max_stack` at all. That is a producer/plumbing change,
   not a verifier change, and adding an `==` rule against saturated limits would
   reject every method. **Do not enable it before the limits are real.**
2. **Operand-stack *depth* at a bci.** `stack.len() <= max_stack` is checkable;
   "equals the JVM's stack depth at this bci" needs the bytecode, which this
   crate does not have. Unreachable from here.
3. **Whether `Undefined` is honest.** A slot that should be
   `MaterializationRequired` but says `Undefined` is indistinguishable at this
   level. Producer fix, tracked in `deopt-metadata.md` §2/§5.7.
4. **Cross-descriptor object aliasing in the monitor list.** Two entries naming
   the same object through *different* descriptors (`RegisterRef(3)` and
   `StackSlotRef(-40)`) are not detected as re-entrancy; the metadata cannot
   express that they alias.
5. **Caller-scope bci is an `invoke`.** A caller scope is parked mid-`invoke` by
   definition, so its bci must name an invoke bytecode. Not checkable without the
   bytecode; and no producer builds caller scopes yet.
6. **The x64 install site is still unwired** (`deopt-metadata.md` §6), so every
   lane in this audit — old and new — is inert for the single-pass backend. Of
   the new ones, `PrimitiveSlotCoveredByOopMap` is the one specifically aimed at
   it: that backend's deopt map and oop map come from different sources.
7. **`ResumeSemanticsMismatch` is a tripwire, not a fix.** Every producer stamps
   `ResumeSemantics::for_reason(reason)` today, so the lane cannot fire on
   current output (`for_reason_agrees_with_the_lane_for_every_reason` pins that).
   Its value is the day a producer starts overriding the derivation — which is
   the point of the field existing — and the day the VM resume sink starts
   *reading* `semantics` instead of re-deriving from `reason`
   (`deopt-metadata.md` §5 item 3). Until then the two consumers cannot diverge
   because only one of them exists.

---

## 7. Tests

`jit/src/deopt.rs::deopt_metadata_soundness_tests` — every construct below
passed verification before this audit:

`a_frame_slot_at_or_above_rbp_is_rejected`,
`an_ordinary_negative_frame_slot_is_accepted`,
`the_outside_frame_message_names_the_slot_and_the_offset`,
`one_word_described_as_two_types_is_rejected`,
`the_same_word_named_twice_with_the_same_type_is_fine`,
`an_int_and_a_long_at_one_word_are_a_conflict`,
`a_primitive_frame_slot_the_oop_map_calls_a_reference_is_rejected`,
`a_primitive_register_the_oop_map_calls_a_reference_is_rejected`,
`an_incomplete_oop_map_does_not_flag_a_primitive_slot`,
`an_oop_map_slot_the_deopt_map_never_names_is_not_an_error`,
`a_monitor_on_a_primitive_is_rejected`, `a_monitor_on_null_is_rejected`,
`reference_shaped_monitor_objects_are_accepted`,
`a_monitor_object_is_checked_against_the_oop_map`,
`a_pending_exception_point_without_rethrow_semantics_is_rejected`,
`a_rethrow_point_whose_reason_routes_it_to_the_resume_stash_is_rejected`,
`for_reason_agrees_with_the_lane_for_every_reason`,
`an_over_deep_owned_chain_is_reported_not_silently_accepted`,
`a_chain_within_the_cap_is_walked_without_complaint`,
`interning_an_over_deep_chain_marks_the_cut_instead_of_dropping_the_outer_frames`,
`a_chain_within_the_cap_still_round_trips_through_interning`,
`the_interned_verifier_names_an_over_deep_chain_rather_than_an_unknown_handle`,
`the_two_resumability_predicates_agree_at_the_cap`.

`jit/src/ir_verify.rs::tests` — `two_snapshots_at_one_bci_are_rejected`,
`distinct_snapshot_bcis_are_accepted`,
`the_duplicate_bci_check_is_part_of_the_frame_state_lane`.

No test uses a wall-clock bound.
