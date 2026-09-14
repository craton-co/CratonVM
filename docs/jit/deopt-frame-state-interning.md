# Immutable, interned frame states

Scope: backlog step 16, *"Make frame states immutable and interned"*
(P0/P1).

Acceptance criterion:

> Safepoint metadata structurally shares states and supports inlining.

Everything described here lives in `jit/src/deopt.rs`. No producer was edited —
`ir_lower.rs`, `x64.rs` and the VM-side construction sites are owned elsewhere
in this pass — so the change is additive and the owned `FrameState` is
untouched. §5 lists the producer edits that would let the interned form become
the *only* form.

---

## 1. Why the owned representation does not scale

`FrameState` owns everything it describes:

```rust
pub struct FrameState {
    pub method_key: String,
    pub bci: u32,
    pub locals: Vec<FrameValue>,
    pub stack: Vec<FrameValue>,
    pub monitors: Vec<MonitorInfo>,
    pub caller: Option<Box<FrameState>>,
}
```

One snapshot per safepoint is therefore one full copy per safepoint, and the
backends put a safepoint at every call, poll and guard. Consecutive safepoints
differ in **one or two slots** — the JVM operand stack moves one value per
bytecode — so the copies are nearly identical.

Two costs compound:

* **Per-method.** `metrics.rs::frame_state_heap_bytes` already accounts for it:
  `method_key.len() + locals.len()*size_of::<FrameValue>() + stack…`, per point.
  `FrameValue` is 56 bytes (its largest variant is `VirtualObject`), so a
  64-local method with 100 deopt points carries ~400 KB of near-duplicate slots.
* **Per-inline-level.** `FrameState::caller` is a `Box<FrameState>`, i.e. a
  *deep copy* of the caller frame into every deopt point of the inlined callee.
  A callee with 40 safepoints inlined into a 16-local caller re-copies those 16
  locals 40 times. This is why interning has to land **before** inlined scope
  chains do (gap §5.2 of `deopt-metadata.md`): otherwise every inline level
  multiplies the metadata.

---

## 2. The representation

```rust
pub struct SharedFrameState {
    pub method_key: MethodKeyId,          // interned string
    pub bci: u32,
    pub locals: ValuesId,                 // interned value array
    pub stack: ValuesId,
    pub monitors: MonitorsId,             // interned monitor array
    pub caller: Option<FrameStateId>,     // parent-linked, shared
    pub semantics: ResumeSemantics,       // explicit reexecute / rethrow
}
```

`Copy`, 32 bytes, no owned heap. Handles are indices into a
`FrameStateInterner`, which is content-addressed: **interning the same state
twice returns the same handle**, so `==` on handles is structural equality and a
map keyed by handle is a map keyed by frame state.

### Structural sharing: chunked value arrays

A `ValuesId` names a *spine* of interned **chunks** of `FRAME_VALUE_CHUNK` (= 8)
values. Chunk `i` always covers slots `[8i, 8i+8)`, so only the last chunk may
be short and a slot index maps to a chunk by division.

Two snapshots that differ in one slot share every chunk but one:

```
snapshot k    spine = [c0, c1, c2, c3, c4, c5, c6, c7]   64 locals
snapshot k+1  spine = [c0, c1, c2, c3, c4, c5, c6, c8]   one slot changed
                                                   ^^ the only new storage
```

The cost of a derived snapshot is one chunk (8 values) plus one spine
(`n` × `u32`), not one array (64 values). Smaller chunks share more and cost
more spine entries; 8 keeps the spine at ~5 % of the slot bytes it indexes for
the array lengths real methods produce.

### The persistent update

```rust
let next = interner.with_local(prev, index, value);   // O(1 chunk)
let next = interner.with_bci(next, bci);              // O(0 slots)
let next = interner.with_caller(next, Some(caller));  // O(0 slots)
```

Each returns a *new* handle; nothing is mutated. `with_local` writing the value
already present, or `with_bci` writing the same bci, returns the input handle
unchanged. A unit test pins the property that matters for a producer switching
from rebuilding owned snapshots to deriving them:
`with_local(id, i, v)` and `intern(&modified_owned_state)` land on the **same
handle**.

### The reexecute flag

`ResumeSemantics { reexecute, rethrow_exception }` replaces the prose convention
that `deopt-metadata.md` §1 used to record as an outright gap ("Reexecute flag —
absent / absent. No field exists."). That row now reads *emitted, unread*: the
field reached `DeoptimizationPoint` (§5.2) and every producer stamps it; the VM
resume sink is the consumer still to switch. Two independent flags, as in a real
scope descriptor:

| | `reexecute` | `rethrow_exception` | meaning |
| --- | --- | --- | --- |
| `RESUME` | false | false | the bytecode at `bci` has taken effect; continue after it |
| `REEXECUTE` | true | false | the guard fired *before* the bytecode; run it from the top |
| `RETHROW` | false | true | not a resume point: route the pending exception through the exception table, starting at the throwing `bci` |

`ResumeSemantics::for_reason(reason)` writes the current convention down in
exactly one place: every reason the two backends emit stamps a guard that fires
before the bytecode it protects (a null check before the field access, a bounds
check before the array access, a div-by-zero test before the `idiv`, an uncommon
trap on a never-taken branch, an OSR-exit at a loop bci that has not run), so
they all re-execute; `DeoptReason::PendingException` — whose own documentation
says it "is **not** a resume point" — rethrows.

**Why the flag is load-bearing for inlining.** The innermost (trapping) scope of
an inlined chain re-executes its bytecode, but every **caller** scope is parked
mid-`invoke`: its `bci` names a call already in progress, and re-executing it
would call the callee a second time. `ResumeSemantics::for_caller_scope()`
(= `RESUME`) is that answer, and `FrameStateInterner::intern` applies it
automatically to every scope it links as a caller. Without the flag, a caller
scope and a trapping scope at the same bci are indistinguishable.

Semantics are part of a state's identity, so the same slots under three
different semantics are three handles sharing one copy of the slots.

---

## 3. The compatibility layer

Producers are not edited in this pass, so both directions are explicit:

| direction | call | used by |
| --- | --- | --- |
| owned → interned | `FrameStateInterner::intern` / `intern_with` / `intern_for_reason` / `intern_point` | anything that already builds a `FrameState` |
| interned → owned | `FrameStateInterner::materialize` / `materialize_point` | the resume sinks, `reconstruct_frame`, `DeoptVerifier` |

`intern` walks the whole `caller` chain iteratively (capped at
`MAX_SCOPE_CHAIN` = 256, so a malformed chain cannot loop or overflow a stack)
and gives every caller scope `for_caller_scope()` semantics.

`materialize` rebuilds the owned `FrameState` including the caller chain. It
still **drops** `ResumeSemantics`, because the owned `FrameState` has no room
for it — but `materialize_point` does not: `DeoptimizationPoint` now carries a
`semantics` field, so the point round-trips, falling back to
`ResumeSemantics::for_reason(point.reason)` only for a handle the interner never
issued. Per-*caller-scope* semantics are still lost by a `FrameState`-level
materialization; only the innermost scope's survive (§5.2).

The install-time verifier gained `DeoptVerifier::violations_interned` /
`verify_interned`. They are deliberately **not** a second implementation of the
rules: each interned point is materialized and handed to the existing
`DeoptVerifier::violations`. Two checkers that were supposed to agree and
drifted would be a worse defect than the materialization this costs, and this
runs once per compile on the install path. A test asserts error-for-error parity
between the two lanes.

A handle the interner never issued is reported as
`DeoptMetadataError::UnknownFrameStateHandle`, never silently skipped: an
unreadable scope chain must not read as "clean".

Predicates are available without materializing — `is_resumable`,
`chain_is_resumable`, `count_materialization_required`, `local`, `stack_slot`,
`locals_len`, `monitors`, `depth`. `is_resumable` is scope-local, matching
`frame_state_is_resumable` exactly; `chain_is_resumable` is the whole-chain
answer an inlined deopt actually needs, which the owned predicate cannot
express.

Every accessor tolerates a foreign handle (returning `None`, an empty slice, or
a conservative `false` for resumability) instead of panicking — this type is
reachable from deopt paths, where a panic is strictly worse than a refusal. No
new panic, no new `unwrap`, and failures surface as a `Bailout`
(`BailoutReason::DeoptMetadata`), never as an abort.

---

## 4. Measured sharing

Measured by construction in `deopt.rs`'s `frame_state_interning_tests`, on
`FrameValue` = 56 bytes.

### 100 safepoints, 72 slots, one slot changing per bci

`sharing_ratio_on_a_hundred_snapshots_differing_by_one_slot`: 64 locals +
8 stack slots, 100 consecutive bcis, exactly one local differing between
consecutive snapshots.

| | owned `Vec`s | interned |
| --- | --- | --- |
| slots stored | 7 200 | **864** |
| slot bytes | 403 200 | **51 588** (48 384 chunk + 3 204 spine) |
| distinct states | 100 | 100 handles (400 B) |
| method keys | 100 copies | 1 |

**88.0 % of slots shared; 87.2 % of slot bytes saved.** The test asserts
`> 85 %` and `> 80 %` respectively, and pins the exact chunk arithmetic
(`chunks == 9 + 99`, `stored_slots == 64 + 8 + 99×8`) so a regression in the
sharing shows up as a hard failure rather than a drifting number.

### The small case

`states_differing_in_one_slot_share_every_other_chunk`: two 32-slot snapshots
differing in one slot store **40** slots, not 64 — 37.5 % shared. Short arrays
share less because the changed chunk is a larger fraction of the array; the
ratio improves monotonically with frame size, which is the direction real
methods go.

### Inlining, depth 1

`one_inlined_caller_scope_is_shared_by_every_point_in_the_callee`: one 17-slot
caller scope, 40 deopt points in the inlined callee (8 locals each).

| | owned `Box<FrameState>` chains | interned |
| --- | --- | --- |
| chain slots | 1 000 (40 × (8 + 17)) | **337** |

**66.3 % shared at inline depth 1**, and the saving grows with depth and with
the number of safepoints in the callee, because the caller's slots are stored
once no matter how many points link to them. `owned_chain_slots` is the honest
denominator here: `InterningStats::logical_slots` counts each distinct *scope*
once, which understates what `Box<FrameState>` actually costs.

### Dedup

Identical states collapse: `InterningStats::state_dedup_ratio` reports the
fraction of intern requests answered by an existing scope. Two backends that
emit the same frame at two native offsets (a common shape — a guard and the
call it protects) now cost one state.

---

## 5. Producer edits (status)

This section listed edits that were **not** made in the pass that wrote this
doc. Two of the three have since landed, in part. Each item now states what is
done, what is not, and what is left.

### 5.1 Populate the caller chain — *machinery landed, no producer*

**Done.** The scope record and the lowerer's consumer both exist:

* `jit/src/ir.rs` — `InlineScopeTable` holds `(method_key, caller_bci, parent)`
  scopes plus a snapshot-index → scope binding (`bind_snapshot`,
  `bind_snapshot_range`, `snapshot_scope`). It is deliberately a **side table**
  rather than a field on `SafepointSnapshot`, which is unchanged
  (`{ bci, locals, stack }`). `push_scope` refuses a foreign/forward parent and
  anything past `MAX_INLINE_SCOPE_DEPTH`, so a malformed chain cannot be built.
* `jit/src/ir_lower.rs` — `Lowerer::resolve_frame_state` fills `caller` from
  `Lowerer::caller_chain_for(index)`, where `index` is the snapshot's position
  in `Graph::safepoints` (which is why the by-bci lookup is now a `position`
  rather than a `find`). `lower_inner_with_scopes` takes the table;
  `lower_inner` passes an empty one, which is byte-identical to the old
  behaviour.
* Undescribed caller frames fail closed: a scope with no resolvable snapshot
  lowers to `[FrameValue::Unsupported]`, not to a plausible-looking empty frame.

**Not done — nothing populates the table.** `push_scope` has no caller outside
`ir_lower.rs`'s and `ir.rs`'s own tests, and the production compile path
(`jit/src/lib.rs`, phase 7) calls `lower_inner`. So `FrameState::caller` is
`None` in every artifact this VM installs today, exactly as before.

The remaining edits:

* `jit/src/lib.rs` — `inline_sites: HashMap<usize, InlineSite>` is keyed by the
  **caller pc** and `InlineSite` carries `class_name`/`method_name`/`descriptor`,
  so the caller bci and the callee key are both available at the splice point.
  What is missing is calling `push_scope` there, `bind_snapshot_range` over the
  contiguous run of snapshots the splice appends, and then
  `lower_inner_with_scopes` instead of `lower_inner`.
* `jit/src/x64.rs` — `build_and_record_deopt_point` still hard-codes
  `caller: None`. The single-pass backend builds its snapshot from
  `local_oop_masks` / `local_kinds` for the *enclosing* method only and has no
  scope stack at all; it needs one pushed at the splice and popped at the
  callee's return.

Once a producer exists, the interned form makes the edit cheap because the
caller scope is interned once:

```rust
let caller = interner.intern_scope(caller_key, caller_bci, &caller_locals,
                                   &caller_stack, &caller_monitors, parent,
                                   ResumeSemantics::for_caller_scope());
// …then, per callee safepoint:
let state = interner.intern_scope(callee_key, bci, &locals, &stack, &monitors,
                                  Some(caller), ResumeSemantics::REEXECUTE);
```

`DeoptVerifier` already walks and checks caller scopes at every depth
(`inlined_caller_scopes_are_checked_too`), and
`reconstruct_frame_from_machine_state` already flattens them into
`ReconstructedFrame::caller_frames`, so the consumer side needs no change.

### 5.2 Populate the reexecute flag — *field landed, consumer not switched*

**Done.** `DeoptimizationPoint` now carries
`semantics: ResumeSemantics` (`jit/src/deopt.rs`, alongside `frame_state`), and
every construction site stamps it:

| site | value |
| --- | --- |
| `jit/src/ir_lower.rs` — inline-callee deopt service point, boxed guard point, `build_deopt_points` | `ResumeSemantics::for_reason(reason)` |
| `jit/src/x64.rs` — `build_and_record_deopt_point` | `ResumeSemantics::for_reason(reason)` |
| `vm/src/vm.rs`, `vm/src/runtime/interpreter.rs` — test/bootstrap and resume-side synthetic points | `ResumeSemantics::REEXECUTE` |

`for_reason` is exactly today's per-`DeoptReason` prose convention, so nothing
changed behaviourally — what changed is that the convention now lives in one
place and `FrameStateInterner::materialize_point` round-trips it instead of
dropping it.

**Not done — the VM resume sink still ignores the field.**
`vm/src/runtime/interpreter.rs` (around the `fv_to_value` frame build) continues
to infer re-execute-vs-resume from `DeoptReason`. Until it reads
`point.semantics`, a producer that knows better — a caller scope parked
mid-`invoke`, or a genuine post-call resume point — still cannot say so in a way
that changes what the interpreter does. That is the one edit left for this item,
and it is the one that makes §5.1's caller scopes correct rather than merely
present.

### 5.3 Switching producers onto the interned form

Not required for this step and not attempted. The shape would be: one
`FrameStateInterner` per compilation, held next to the `deopt_points` vector;
producers call `intern_scope` instead of building `FrameState`; the artifact
stores `Vec<InternedDeoptPoint>` plus the interner; `x64_deopt_entry` /
`ir_deopt_entry` call `materialize` (or, better, resolve straight from the
interned form). The blocking dependency is that `DeoptimizationPoint` is baked
by raw pointer into the x64 deopt stub (`x64.rs:2644`, `Box::new(point.clone())`
whose payload address is baked as an imm64), so the interner would have to
outlive the artifact exactly as the boxes do today.

---

## 6. To reconcile

* ~~**`docs/jit/deopt-metadata.md` §6, last paragraph is stale.**~~ **Fixed.**
  That section no longer claims `deopt_metadata_bailout` reuses
  `BailoutReason::IrVerification` with a `phase=deopt-metadata` context. The
  facts, verified: `BailoutReason::DeoptMetadata(String)` exists
  (`jit/src/bailout.rs:131`, category `"deopt_metadata"` at `:154`, in
  `all_reasons()` at `:376`), and `deopt_metadata_bailout`
  (`jit/src/deopt.rs:4046`) uses it with context **`phase=install`**.
  `bailout.rs`'s own `deopt_metadata_is_its_own_category_and_counter` test pins
  that it is a distinct bucket, not an alias.
* ~~**`deopt-metadata.md` §1 "Reexecute flag" and §5.3**~~ **Fixed.**
  `DeoptimizationPoint::semantics` landed, so the completeness table's row now
  reads *emitted, unread* and names the one consumer left (§5.2).
* **STILL OPEN — the single-pass install site.** `deopt-metadata.md` §6's second
  bullet: `jit/src/x64.rs` still installs its deopt metadata without running
  `DeoptVerifier`. The IR lowerer's install-time check is wired; this one is not.
* **`MAX_SCOPE_CHAIN` truncation.** Unchanged, and still open. A caller chain
  deeper than 256 is silently truncated by `intern`/`materialize`. That is far
  beyond any inline depth the planner can produce
  (`MAX_INLINE_BYTECODE_SIZE` budgeting bounds it long before), but if a future
  inliner can exceed it, the truncation must become a `Bailout` rather than a
  shorter chain. Note this is a *different* bound from
  `MAX_INLINE_SCOPE_DEPTH`, which `InlineScopeTable::push_scope` enforces by
  refusing the scope outright.
* ~~**`is_resumable` vs `chain_is_resumable`.**~~ **Fixed on the owned side.**
  `frame_state_is_resumable` now follows `caller`, pinned by
  `resumability_follows_the_whole_caller_chain`. Its two callers — `x64.rs`'s
  unresumable-indy-trap bail and `CompiledMethod::osr_exit_policy` — therefore
  answer "no" for an artifact whose trap sits under a caller frame nobody can
  describe, instead of being admitted by a clean innermost scope. This mattered
  before §5.1's producer lands, not after.
