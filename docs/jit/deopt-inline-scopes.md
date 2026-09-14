# Inlined scopes and the deopt frame chain

Companion to `deopt-metadata.md` (what the metadata contains) and
`deopt-frame-state-interning.md` (how it is stored). This one covers the two
gaps §5.1 and §5.2 of the interning doc listed as *producer edits required*:

1. **inlined caller scopes** — `FrameState::caller` was hard-coded `None`
   because nothing recorded which inlined scope a safepoint belonged to;
2. **the reexecute flag** — `ResumeSemantics` existed but reached only the
   interned form, so `materialize` dropped it and every consumer re-derived
   re-execute-vs-resume from `DeoptReason`.

Both are closed on the IR side here. Neither changes any compile today: the
scope table is empty on every compile (the IR builder does not inline), and
every producer stamps the semantics the convention already implied.

---

## 1. The scope representation (`jit/src/ir.rs`)

```rust
pub struct InlineScope {
    pub method_key: String,             // the CALLER's key
    pub caller_bci: u32,                // the invoke, already in progress
    pub caller_snapshot: Option<u32>,   // index into Graph::safepoints
    pub parent: Option<InlineScopeId>,  // the caller's caller
}

pub struct InlineScopeTable { /* scopes + snapshot→scope bindings */ }
```

Read a scope as *"the frame parked mid-`invoke` while the scope below it
runs"*. A `SafepointSnapshot` bound to scope `s` is the innermost frame; `s`,
`s.parent`, … are the frames stacked above it.

### Why a side table and not a field

`deopt-frame-state-interning.md` §5.1 proposed
`SafepointSnapshot { …, inline_scope: Option<InlineScopeId> }` plus a per-graph
table on `Graph`. Both break every struct literal of those types in the crate,
and there are a lot of them in files owned by other passes:

| type | literal sites that would need a new field |
| --- | --- |
| `ir::SafepointSnapshot` | `jit/src/lib.rs:14982`, `:15021`, `:15049`; `jit/src/ir_verify.rs:1682`, `:1701`, `:1743`; `jit/src/ir_optimize.rs:3332`, `:3371`; `jit/src/ir_lower.rs` ×3; `jit/src/ir.rs` ×6 |
| `ir::Graph` | ~20 sites across `lib.rs`, `ir_verify.rs`, `ir_optimize.rs`, `ir_schedule.rs`, `ir_lower.rs`, `ir.rs` |

A table threaded alongside the graph costs one parameter at the lowering entry
point and nothing at all to the hand-built graphs in the test suites. It is
also the shape the producer wants: an inliner appends a *contiguous run* of
snapshots per splice, which is one `bind_snapshot_range` call.

### The index-keying invariant

`by_snapshot` is keyed by the snapshot's index in `Graph::safepoints`. That is
sound because nothing in the pipeline removes or reorders that vector —
`IrBuilder` pushes, `ir_optimize` (`:2041`) and `lib.rs` (`:8936`) rewrite slots
in place, and the lowerer only reads. An index past the end reads back `None`,
so a table built against a shorter graph degrades to "no inlining info" rather
than to a wrong scope. **If a future pass ever compacts `safepoints`, it must
remap the table.**

### Bounds

`MAX_INLINE_SCOPE_DEPTH = 64`, enforced at `push_scope` (HotSpot's own limit is
9). `push_scope` also refuses a parent the table never issued — a parent id is
always strictly smaller than the child's, so chains are acyclic by construction
rather than by defence at every walk.

---

## 2. Consumption (`jit/src/ir_lower.rs`)

* `Lowerer` carries `inline_scopes: &'a InlineScopeTable`.
* `resolve_frame_state(sp, index)` takes the snapshot's *index* (the lookup in
  `resolve_frame_state_for_bci` became a `position`, still first-match-wins).
* `caller_chain_for(index)` builds `FrameState::caller`, outermost last.
* `resolve_frame_values(sp)` is the split-out half that resolves a snapshot's
  `(locals, stack)` without touching scopes — so `caller_chain_for` can resolve
  a caller's snapshot without mutual recursion.
* `lower_inner_with_scopes(…, &InlineScopeTable)` is the new entry point;
  `lower_inner` delegates to it with an empty table.

### Fail-closed on an undescribed caller

A caller frame is **not** empty — its locals and operand stack are live. When a
scope names no `caller_snapshot`, the lowerer emits a frame holding one
`FrameValue::Unsupported`, not an empty one. An empty caller frame is silently
wrong: every resume sink maps a missing slot to `Value::Int(0)`, so the
interpreter would resume the caller with all-zero locals and no error anywhere —
the exact class of failure `FrameValue::MaterializationRequired` exists to
prevent.

---

## 3. `frame_state_is_resumable` now walks the chain

`deopt-frame-state-interning.md` §6 flagged this as the hazard that arrives
*with* caller chains:

> The owned `frame_state_is_resumable` only inspects the innermost scope, which
> is correct today only because no producer builds a chain. Once §5.1 lands, the
> resume sinks must switch to the chain-wide predicate, or a caller scope
> holding a `MaterializationRequired` slot will resume as if it were clean.

The predicate itself now follows `caller`, and it is behaviour-preserving until
a chain exists (a one-scope chain is exactly the old predicate). The walk is
bounded by `MAX_SCOPE_CHAIN`; a deeper chain is refused rather than walked
further.

**Is the hazard reachable? No — twice over, and it is now closed anyway.**

* The scope table is empty on every compile (no IR-path inliner), so nothing
  builds a chain in the first place.
* Even given a chain, the VM's *resume* sinks refuse any `ReconstructedFrame`
  with a non-empty `caller_frames` outright:
  `vm/src/runtime/interpreter.rs:13393` (`build_deopt_frame`),
  `:13616` (`build_deopt_frame_inner`), `:13959` (the OSR-exit sink). An inlined
  chain never resumes today; it always takes the safe whole-method re-run.

The real callers of `frame_state_is_resumable` are the two **compile-time
admission gates**, not resume sinks:

| caller | question it decides |
| --- | --- |
| `jit/src/x64.rs:22109` | unresumable-`invokedynamic`-trap: bail the whole compile |
| `jit/src/lib.rs:2916` (`CompiledMethod::osr_exit_policy`) | may this artifact be OSR-entered at all |

Making the predicate chain-aware is what stops an artifact whose only trap sits
under an undescribable caller scope from being admitted because the *innermost*
frame happens to be clean. (`osr_exit_policy` separately refuses any deopt point
with `caller.is_some()` — `OSR_REFUSE_INLINED_SCOPE` — so the OSR path is
already conservative about chains.)

The interned side keeps both answers on purpose:

| predicate | scope |
| --- | --- |
| `FrameStateInterner::is_resumable` | this scope only — "is *this* frame clean" |
| `FrameStateInterner::chain_is_resumable` | whole chain — the counterpart of the owned `frame_state_is_resumable` |

---

## 4. `DeoptimizationPoint::semantics`

`DeoptimizationPoint` gained `semantics: ResumeSemantics`. Every producer
stamps `ResumeSemantics::for_reason(reason)`, which *is* the convention the
consumers were each re-deriving, so this is behaviour-preserving by
construction. Two consequences:

* `FrameStateInterner::intern_point` reads the field instead of re-deriving it,
  and `materialize_point` restores it instead of dropping it — the interned form
  is now lossless for a flat point.
* A producer that knows better can say so: a caller scope
  (`ResumeSemantics::for_caller_scope()` = `RESUME`) or a genuine post-call
  resume point.

Still dropped by `materialize`: the *caller scopes'* semantics, because the
owned `FrameState` has no field for them. Closing that means either giving
`FrameState` the field too, or retiring the owned form (interning §5.3).

---

## 5. The producer edit (`jit/src/lib.rs`)

Not made here — `lib.rs` is owned elsewhere. It is the only thing standing
between this representation and a real caller chain.

`try_compile_inner` builds `inline_sites: HashMap<usize, InlineSite>`
(`lib.rs:12204`), keyed by **caller pc**, where `InlineSite` (`lib.rs:3226`)
carries `class_name` / `method_name` / `descriptor`. Both halves of a scope are
therefore already at the splice point.

```rust
// once, next to the graph:
let mut inline_scopes = cratonvm_jit::ir::InlineScopeTable::new();

// at each splice of an inlined callee, with `caller_pc` the invoke's bci:
let caller_snapshot = graph
    .safepoints
    .iter()
    .position(|s| s.bci == caller_pc)
    .map(|i| i as u32);
let first = graph.safepoints.len();          // before the callee's body is built
// … build/splice the callee body, appending its snapshots …
let scope = inline_scopes.push_scope(
    &format!("{caller_class}.{caller_name}:{caller_desc}"),
    caller_pc as u32,
    caller_snapshot,
    enclosing_scope,                          // None at the outermost method
);
if let Some(scope) = scope {
    inline_scopes.bind_snapshot_range(first..graph.safepoints.len(), scope);
}

// and at the lowering call (lib.rs's `ir_lower::lower_inner(...)`):
ir_lower::lower_inner_with_scopes(
    graph, schedule, num_params, num_locals, helpers, branch_hints, sr_map,
    direct_calls, ic_slots, compact_fields, &inline_scopes,
)
```

`push_scope` returning `None` (foreign parent, or past the depth cap) means the
snapshots stay unbound and the deopt points stay flat — the same conservative
outcome as not inlining.

> **Status, verified 2026-09-12.** Both halves of the note below have moved.
> `IrBuilder` now inlines by splicing callee bytecode (`IrInlineSite`; gated by
> `CRATONVM_JIT_IR_INLINE`, **default on** since 2026-09-06, `=0` is the kill
> switch). It still registers **no** scope, deliberately: a spliced region
> carries the caller's `invoke` bci with re-execute semantics instead of a
> scope chain (the reasoning is at `IrInlineSite` in `jit/src/ir.rs`). So the
> `InlineScopeTable` stays empty on every compile, and
> `Lowerer::caller_chain_for` yields `None`. The single-pass backend's scope
> stack has landed: its deopt-point publisher (`jit/src/x64/deopt_stubs.rs`) sets
> `frame_state.caller = self.inline_caller_chain()` inside a spliced body. The
> single-pass production entry is `x64::compile_with_param_slots`;
> `x64::compile` is the legacy test wrapper.

**Note the ordering constraint the IR path does not have yet:** the IR builder
(`ir.rs`, `IrBuilder::build`) does not inline at all — `inline_sites` is
consumed by the *single-pass* backend (`x64::compile`). So until an IR-path
inliner exists, the producer above has nowhere to run, and the table stays
empty. The single-pass backend's own scope stack is a separate edit in
`x64.rs:2612` (see `deopt-frame-state-interning.md` §5.1).

---

## 6. To reconcile

* **Cross-scope virtual objects.** `caller_chain_for` resolves each scope's
  values with its own `emitted` set, so a scalar-replaced object live in BOTH a
  callee and its caller is *defined* twice instead of once plus a
  `VirtualObjectRef`, and the materializer would rebuild two objects where the
  program had one. Unreachable while the table is empty; a producer that starts
  building chains must thread one `emitted` set through the whole chain first.
* **`resolve_frame_state_for_bci` is still first-match-wins on `bci`.** With
  inlining, a callee and its caller can carry the same bci, and the guard sites
  (`Op::Guard`, `emit_deopt_unless`) key on bci alone. They need to key on
  `(bci, scope)` before an IR-path inliner lands.
* **`deopt-frame-state-interning.md` §5.1 / §5.2 / §6** now describe work that
  is partly done; the tables there still list `ir_lower.rs:3840/4164/4173` and
  `3079/3970/4377` as open. §6's `is_resumable` vs `chain_is_resumable` bullet
  is resolved.
* **`deopt-metadata.md` §1 "Reexecute flag"** ("absent / absent. No field
  exists.") is now stale for `DeoptimizationPoint`.
* **`MAX_SCOPE_CHAIN` truncation** (interning doc §6) is unchanged: a chain
  deeper than 256 is truncated by `intern`/`materialize`. The owned predicate
  now *refuses* such a chain rather than accepting a truncation, which is the
  fail-closed half; the interner's silent truncation still wants a `Bailout`.
