# Immutable, interned frame states

Scope: backlog step 16, *"Make frame states immutable and interned"*
(`docs/known-issues/deep-research-vm-c2.md`, P0/P1).

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

`ResumeSemantics { reexecute, rethrow_exception }` replaces the prose
convention `deopt-metadata.md` §1 records as an outright gap ("Reexecute flag —
absent / absent. No field exists."). Two independent flags, as in a real scope
descriptor:

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
**drops** `ResumeSemantics`, because the owned type has no room for it — which
is exactly the gap §5 asks producers to close.

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

## 5. Producer edits required (files this pass does not own)

Nothing below was changed here. Each item is the exact edit needed.

### 5.1 Populate the caller chain

`FrameState::caller` is hard-coded `None` at four producer sites:

| file:line | site |
| --- | --- |
| `jit/src/ir_lower.rs:3840` | `resolve_frame_state_for_bci`, the no-snapshot fallback frame |
| `jit/src/ir_lower.rs:4164` | `resolve_frame_state`, the scalar-replacement path |
| `jit/src/ir_lower.rs:4173` | `resolve_frame_state`, the ordinary path |
| `jit/src/x64.rs:2632` | `build_and_record_deopt_point` |

The blocker is upstream of all four: **nothing records which inlined scope a
safepoint belongs to.**

* `jit/src/ir.rs:884` — `SafepointSnapshot { bci, locals, stack }` has no
  inline-scope field. It needs one, e.g.
  `inline_scope: Option<InlineScopeId>`, plus a per-graph table of
  `(method_key, caller_bci, parent)` scopes. The IR path cannot build a chain
  before this exists.
* `jit/src/lib.rs:12192` — `inline_sites: HashMap<usize, InlineSite>` is keyed
  by the **caller pc**, and `InlineSite` (lib.rs:3226) carries
  `class_name` / `method_name` / `descriptor`. So the caller bci and the callee
  key are both available at the splice point; what is missing is threading them
  into the snapshot that the splice emits.
* `jit/src/x64.rs:2612` — the single-pass backend builds the snapshot from
  `local_oop_masks` / `local_kinds` for the *enclosing* method only. It needs
  the same scope stack, pushed at the splice and popped at the callee's return.

Once a scope stack exists, the producer edit is mechanical and cheap because
the caller scope is interned once:

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

### 5.2 Populate the reexecute flag

`ResumeSemantics` currently reaches only the interned form; the owned
`DeoptimizationPoint` still has no field for it, so `materialize` drops it. To
make it end-to-end, `DeoptimizationPoint` needs a `semantics: ResumeSemantics`
field (or a `reexecute: bool` + `rethrow_exception: bool` pair), which touches
every construction site:

| file:line | site | value to pass |
| --- | --- | --- |
| `jit/src/ir_lower.rs:3079` | inline-callee deopt service point | `ResumeSemantics::for_reason(reason)` |
| `jit/src/ir_lower.rs:3970` | boxed guard point | `ResumeSemantics::for_reason(reason)` |
| `jit/src/ir_lower.rs:4377` | `build_deopt_points` | `ResumeSemantics::for_reason(reason)` |
| `jit/src/x64.rs:2605` | `build_and_record_deopt_point` | `ResumeSemantics::for_reason(reason)` |
| `vm/src/vm.rs:74958`, `74980` | test/bootstrap points | `ResumeSemantics::REEXECUTE` |
| `vm/src/runtime/interpreter.rs:14826` | resume-side synthetic point | `ResumeSemantics::REEXECUTE` |

`for_reason` is the drop-in that keeps behaviour byte-identical to today's
convention; each producer should then override it where it knows better (a
caller scope, or a point whose bci is a genuine post-call resume).

The consumer that must read it is the VM resume sink
(`vm/src/runtime/interpreter.rs`, around the `fv_to_value` frame build): today
it infers re-execute-vs-resume from `DeoptReason`, and every new reason has to
re-learn the convention.

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

* **`docs/jit/deopt-metadata.md` §6, last paragraph is stale.** It says
  "Until then `deopt_metadata_bailout` reuses `BailoutReason::IrVerification`
  with a `phase=deopt-metadata` context." That is no longer true:
  `BailoutReason::DeoptMetadata(String)` exists (`jit/src/bailout.rs:131`, with
  category `"deopt_metadata"` at `:154`) and `deopt_metadata_bailout` uses it
  with context `phase=install`. The doc comment on `deopt_metadata_bailout` in
  `deopt.rs` carries the same stale claim and should be corrected in the same
  pass that owns it.
* **`deopt-metadata.md` §1 "Reexecute flag" and §5.3** should be updated once
  the field reaches `DeoptimizationPoint`; until then the flag exists only on
  the interned side, which the completeness table does not yet mention.
* **`MAX_SCOPE_CHAIN` truncation.** A caller chain deeper than 256 is silently
  truncated by `intern`/`materialize`. That is far beyond any inline depth the
  planner can produce (`MAX_INLINE_BYTECODE_SIZE` budgeting bounds it long
  before), but if a future inliner can exceed it, the truncation must become a
  `Bailout` rather than a shorter chain.
* **`is_resumable` vs `chain_is_resumable`.** The owned
  `frame_state_is_resumable` only inspects the innermost scope, which is
  correct today only because no producer builds a chain. Once §5.1 lands, the
  resume sinks must switch to the chain-wide predicate, or a caller scope
  holding a `MaterializationRequired` slot will resume as if it were clean.
