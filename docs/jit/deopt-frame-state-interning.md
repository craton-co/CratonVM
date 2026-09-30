# Immutable, interned frame states

**Status: RETIRED 2026-09-21** (round 10, lane `interner`). The implementation
this document described — `FrameStateInterner`, `SharedFrameState`,
`InternedDeoptPoint`, `InterningStats`, the four handle newtypes, the chunked
value spines, the persistent-update family, the materialization layer, and
`DeoptVerifier::violations_interned` / `::verify_interned` — was deleted from
`jit/src/deopt.rs`. At deletion it had **zero references anywhere outside its own
`#[cfg(test)]` modules**, in that file or any other. §7 is the argument;
`docs/internal/fixed-bugs/r10-deoptverify-frame-state-interner-has-no-production-user-RETIRED-20260922.md`
is the page that forced the decision, and it records what round 10 wave 9 then
closed — all three of §7.5's residuals, including §5.2 below.

This document stays because the backlog item does. §1 (the cost the owned
representation pays) is unchanged and is still the reason to do this work; §2–§4
are the record of one design and one set of *construction* measurements, kept so
stage 2 does not start from nothing; §5 is the producer status, which is what
made the answer "not yet, and therefore not now". **Nothing in §2, §3 or §4
describes code that exists today** — read them as a design note, not as an API
reference. §5 is corrected for the 2026-09-22 retirement of the IR-side scope
table and for §5.2's consumer edit, both of which landed after this status line
was written; §7.5 lists what the deletion did not close and what then closed it.

Scope: backlog step 16, *"Make frame states immutable and interned"*
(P0/P1).

Acceptance criterion:

> Safepoint metadata structurally shares states and supports inlining.

Everything described below lived in `jit/src/deopt.rs`. No producer was ever
edited — `ir_lower.rs`, `x64.rs` and the VM-side construction sites are owned
elsewhere — so the change was additive and the owned `FrameState` was untouched
throughout. §5 lists the producer edits that would have let the interned form
become the *only* form; none of them landed.

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
`verify_interned`. They were deliberately **not** a second implementation of the
rules: each interned point was materialized and handed to the existing
`DeoptVerifier::violations`. Two checkers that were supposed to agree and
drifted would be a worse defect than the materialization this costs, and this
runs once per compile on the install path. A test asserted error-for-error parity
between the two lanes.

**Both were deleted with the rest.** That comment's own reasoning is why: it is
the argument for one implementation, and by wave 6 of round 10 the verifier was
armed on BOTH backends' install paths while `violations_interned` had never been
called by anything but its parity test. A front door nothing walks, protecting a
representation nothing builds, held in step by one test — the honest state is one
front door. `DeoptMetadataError::UnknownFrameStateHandle`, which only
`violations_interned` could produce, went with it;
`DeoptMetadataError::ScopeChainTooDeep` did **not**, because
`DeoptVerifier::check_point` still produces it for an owned chain past
`MAX_SCOPE_CHAIN`.

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

**Read this caveat before quoting any number below.** These were measured *by
construction*: each figure comes from a unit test that BUILDS a snapshot sequence
with a chosen shape (usually "exactly one local differs between consecutive
bcis") and then derives the successors with `with_local`. That is a correct
measurement of the representation and **not** a prediction for a compile, because
no producer derives snapshots that way:
`x64/deopt_stubs.rs::build_and_record_deopt_point` rebuilds the whole frame from
regalloc provenance at each site, and `ir_lower`'s `build_deopt_points` rebuilds
it from the IR. A production interner would have got whatever the content-hash
dedup found, which nobody ever measured — `InterningStats` had no production
feeder, so every one of its getters read zero for the whole life of the feature.
That gap is §7's first argument.

Measured by construction in `deopt.rs`'s (now deleted)
`frame_state_interning_tests`, on `FrameValue` = 56 bytes.

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

### 5.1 Populate the caller chain — *IR machinery RETIRED 2026-09-22, no producer*

**The IR half of this entry no longer exists.** Round 10 wave 9 deleted
`ir::InlineScopeTable`, `InlineScope`, `InlineScopeId` and
`ir_lower::Lowerer::caller_chain_for`; `resolve_frame_state` now sets
`caller: None` unconditionally and says so, and `lower_inner_with_scopes`
collapsed into `lower_inner_with_array_list`. What follows is kept as the record
of the design, because the backlog item is unchanged — but **none of it
describes code that exists**.

What it was: a side table holding `(method_key, caller_bci, caller_snapshot,
parent)` scopes plus a snapshot-index → scope binding, read by
`Lowerer::caller_chain_for` to fill `FrameState::caller`. Deliberately a side
table rather than a field on `SafepointSnapshot`, which is unchanged
(`{ bci, locals, stack }`). `push_scope` refused a foreign/forward parent and
anything past `MAX_INLINE_SCOPE_DEPTH` (that constant survives — it bounds
`IrInlineFrameSites`' stack-trace walks). Undescribed caller frames failed
closed: a scope with no resolvable snapshot lowered to
`[FrameValue::Unsupported]`, never to a plausible-looking empty frame. That
fail-closed rule is the one piece a re-landing must keep, and it is written up
in `docs/jit/deopt-inline-scopes.md`.

**Why it went.** Nothing ever populated it. `push_scope`, `bind_snapshot` and
`bind_snapshot_range` had no caller outside `#[cfg(test)]` anywhere in the
workspace, so `is_empty()` was true on every compile and `FrameState::caller`
was `None` in every artifact this tier installed — while the READ side sat on the
production lowering path, so a `rg` for `InlineScopeTable` found production hits
and reported inlined-scope support this tier has never had. That is a worse
position than the interner's, which a `rg` told the truth about.

**And a side table was never the missing half.** `IrBuilder::build` walks ONE
method's bytecode; when it splices (`IrInlineSite`) it deliberately carries the
CALLER's `invoke` bci with re-execute semantics rather than a scope chain,
because re-execution needs no frame identity while a chain needs the innermost
frame's — which `resolve_frame_state` leaves for the VM to fill from the running
`CompiledMethod`, i.e. it would name the caller. A producer has to answer the
identity question first; the table was the easy part.

The remaining edits, if a producer is ever written:

* `jit/src/lib.rs` — `inline_sites: HashMap<usize, InlineSite>` is keyed by the
  **caller pc** and `InlineSite` carries `class_name`/`method_name`/`descriptor`,
  so the caller bci and the callee key are both available at the splice point.
  A scope record would have to be rebuilt (it is ~250 lines) and bound over the
  contiguous run of snapshots the splice appends.
* the identity question above, which is the substantive half.
* `jit/src/x64.rs` — the CONCLUSION here still holds, but two of its premises
  are out of date, as of changes that landed 2026-08-18 and were only noticed in
  round 10. `build_and_record_deopt_point` does not "hard-code `caller: None`",
  and the single-pass backend does not have "no scope stack at all" — it has
  `inline_sites`, and `build_frame_state_at` stamps an identity from it. What is
  still true, and is the whole point of the entry, is that nothing publishes a
  deopt point from INSIDE a splice, so no artifact this VM installs carries a
  caller chain from this backend either.
  Round 10 also found a defect hiding behind the stale wording: because
  `build_frame_state_at` fills `self.num_locals` — the ROOT method's count, from
  the root's local homes — whatever identity it stamps, a point published from
  inside a splice would name the callee and carry the CALLER's locals. So the
  edit this entry asks for is not merely "push a scope"; see
  `docs/internal/fixed-bugs/r10-deoptverify-splice-published-point-would-carry-the-callers-locals-FIXED-20260922.md`,
  which also records that `inlining.rs`'s claim that "the FIRST edit to publish
  one produces well-formed metadata" is false on exactly this axis.

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
`ReconstructedFrame::caller_frames`, so the consumer side needs no change. Since
round 10 wave 9 the VM side can also RESUME one:
`deopt_resume::materialise_inlined_chain` pushes the chain outermost-first and
`scope_resume_pc` parks each scope by its own `ResumeSemantics`, so the caller
scopes land after their invokes rather than on them.

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

**DONE 2026-09-22 (round 10 wave 9).** Every routing and resume decision now
reads `semantics`; `reason` is the charging key and nothing else. Specifically:

* both trampoline entries (`ir_deopt_entry`, `x64_deopt_entry`) route an
  exceptional frame to `LAST_EXCEPTIONAL` on `semantics.rethrow_exception`
  rather than on `reason == PendingException`. Equivalent on any installed
  artifact — `DeoptMetadataError::ResumeSemanticsMismatch` refuses a point where
  the two disagree, on both backends' install paths in release builds — but no
  longer equivalent for a hand-built or future point, where the flag is right
  and the reason is a guess;
* `transfer_osr_exception_exit_into_live_frame`'s provenance check, same swap;
* `build_deopt_frame_inner` and `build_deopt_frame_chain` ask
  `rframe.semantics.reexecute` before parking anything at `rframe.bci`, instead
  of trusting `ordinary_stash_frame`'s gate two crates away
  (`DeoptFrameBail::NotAResumePoint`, expected zero);
* `deopt_resume::scope_resume_pc` decides where each scope of a chain parks from
  that scope's own semantics rather than from its position — which is what makes
  §5.1's caller scopes correct rather than merely present, and is the sentence
  this paragraph used to end with.

**And the per-scope half is answered too.** The owned `FrameState` still has no
semantics field, so a `DeoptimizationPoint` carries only the innermost scope's —
but `ReconstructedFrame` now carries one PER FRAME, stamped at the unwind:
`reconstruct_frame` and `reconstruct_frame_from_machine_state` give the trapping
frame the point's semantics and every linked caller
`ResumeSemantics::for_caller_scope()`, which is exactly what the retired interned
form did and is not a guess (a caller scope's bci names an `invoke` that is in
progress by construction). What is still owed to a future producer: if a caller
scope ever needs to be something other than `RESUME`, the METADATA has to carry
it. Nothing in either backend can express that today.

**What is deliberately not done:** honouring a `RESUME` point by parking after
its bci in the ordinary sink. `LAST_DEOPT` admits re-execute points only, and the
stash's nesting argument — why a frame under the top can only be a stale
leftover — depends on that. A producer of genuine post-call resume points has to
answer that argument first; the refusal is load-bearing, not an omission.

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
* ~~**the single-pass install site.**~~ **Fixed 2026-09-21 (round 10 wave 6).**
  This bullet said `jit/src/x64.rs` installed its deopt metadata without running
  `DeoptVerifier`. It does now: `x64/driver.rs`'s finalize path calls
  `deopt::verify_deopt_metadata` over `cm.deopt_points`, with real
  `MethodFrameLimits` for the compiling method and for every inlined callee, and
  **refuses the artifact** on a violation (`note_jit_bail_site_at
  ("deopt-metadata-violation")`) rather than demoting it. Both backends now check
  on the install path, in release builds, which is what lets every consumer read
  `semantics` and trust that it agrees with `reason` (§5.2).
* ~~**`MAX_SCOPE_CHAIN` truncation.**~~ **No longer open, and the reason is worth
  recording.** This bullet used to say a chain deeper than 256 was *silently
  truncated* by `intern`/`materialize`. By the time the interner was retired that
  was already false — `intern_with` terminated a cut chain with an explicitly
  unreconstructable sentinel scope and `materialize` refused outright — and both
  of those paths are now gone anyway. What survives is the owned side, and it
  fails closed at the cap in both directions:
  `frame_state_is_resumable` answers `false` at the cap rather than `true`, and
  `DeoptVerifier::check_point` reports
  `DeoptMetadataError::ScopeChainTooDeep` rather than checking a 256-scope prefix
  and calling it clean. `MAX_SCOPE_CHAIN` is retained in `jit/src/deopt.rs` for
  exactly those two walks. It remains a *different* bound from
  `MAX_INLINE_SCOPE_DEPTH`, which since the 2026-09-22 retirement of
  `InlineScopeTable` bounds only `ir::IrInlineFrameSites`' stack-trace walks.
* ~~**`is_resumable` vs `chain_is_resumable`.**~~ **Fixed on the owned side.**
  `frame_state_is_resumable` now follows `caller`, pinned by
  `resumability_follows_the_whole_caller_chain`. Its two callers — `x64.rs`'s
  unresumable-indy-trap bail and `CompiledMethod::osr_exit_policy` — therefore
  answer "no" for an artifact whose trap sits under a caller frame nobody can
  describe, instead of being admitted by a clean innermost scope. This mattered
  before §5.1's producer lands, not after.

---

## 7. Retirement (2026-09-21, round 10, lane `interner`)

The subsystem was deleted. This is the argument, and it is deliberately an
argument about *what could be measured* rather than about line count.

### 7.1 What the deletion removed

From `jit/src/deopt.rs`, 2 065 lines: **1 119 of production code and 946 of
tests** (counted from the deleted ranges). The file went from 9 812 lines to
7 902, the difference being the retirement prose that replaced them.

| Removed | |
| --- | --- |
| `FrameStateInterner` | the store, its ten private index maps, and its whole public API (`intern`, `intern_with`, `intern_for_reason`, `intern_scope`, `intern_point`, the accessors, `with_local`/`with_stack_slot`/`with_bci`/`with_caller`/`with_semantics`, `replace_slot`, `materialize`, `materialize_point`, `is_resumable`, `chain_is_resumable`, `count_materialization_required`, `owned_chain_slots`, `stats`) |
| `SharedFrameState`, `InternedDeoptPoint` | the interned scope and point |
| `FrameStateId`, `ValuesId`, `MonitorsId`, `MethodKeyId`, `ChunkId` | the handle newtypes |
| `InterningStats` + `slot_sharing_ratio` / `owned_slot_bytes` / `interned_slot_bytes` / `slot_byte_saving` / `state_dedup_ratio` | the measurement that never got a production number |
| `FRAME_VALUE_CHUNK`, `hash_frame_value`, `hash_frame_values`, `hash_monitors`, `monitors_eq`, `count_materialization_required_in` | the chunking and the structural hashing |
| `DeoptVerifier::violations_interned`, `::verify_interned` | the second verifier front door |
| `DeoptMetadataError::UnknownFrameStateHandle` | the only error `violations_interned` could produce |
| `mod frame_state_interning_tests` (most of it) | 21 tests; the four owned-predicate tests at its tail survive in `mod frame_state_resumability_tests` |
| 4 tests in `mod deopt_metadata_soundness_tests` | the interner-side halves of the scope-cap pairs; the owned halves stay |

What stayed, on purpose: **`ResumeSemantics` in full** (it is a real
`DeoptimizationPoint` field, stamped by every producer, and the verifier's
exception-state-agreement lane checks it), **`MAX_SCOPE_CHAIN`** (two owned walks
need it), **`DeoptMetadataError::ScopeChainTooDeep`** (`check_point` produces it),
and the owned resumability predicates.

### 7.2 The measurement the feature existed to justify could not be taken

Interning is a memory optimisation, so the only thing that can settle whether it
is worth a second representation is a number. `InterningStats` is how that number
is read, and with no production interner there was no production number — for the
entire life of the feature, every one of its five ratio getters read zero. Zero
here is indistinguishable from "this never happened", which is round 10's
defect-of-the-round in its purest form: nine lanes found the same shape this
round, and the ratchet built for it is `scripts/check-orphan-instruments.sh`.

The numbers in §4 do not fill that gap, and §4 now says so at the top: they were
measured by *constructing* the ideal input (one slot changing per bci, derived
with `with_local`). No producer derives; both of them rebuild each frame
independently, so the sharing a production interner would have found is a
different, unknown number.

### 7.3 The saving it was designed for is blocked on work nobody has done

§1's second cost — the per-inline-level one — is the structural one, and it is the
reason the design says interning must land *before* inlined scope chains.
`FrameState::caller` is a `Box<FrameState>`, so an inlined caller's slots are
deep-copied into every deopt point of the callee; interning collapses that to one
handle, and the saving grows with depth. That multiplier is **1 today**, because
no artifact this VM installs carries a caller chain at all:

* single-pass — `build_and_record_deopt_point` does set
  `frame_state.caller = self.inline_caller_chain()` (that edit landed 2026-08-18),
  but `x64/inlining.rs`'s splice postcondition rolls back any splice that
  published a point, and the sink those traps reach
  (`build_deopt_frame_inner`) refuses a non-empty chain outright
  (`DeoptFrameBail::InlinedChain`). Worse, round 10 found that a point published
  from inside a splice would name the callee and carry the ROOT method's locals —
  `docs/internal/fixed-bugs/r10-deoptverify-splice-published-point-would-carry-the-callers-locals-FIXED-20260922.md`;
* IR tier — `ir::InlineScopeTable` exists and `ir_lower::lower_inner_with_scopes`
  consumes it, but `push_scope`/`bind_snapshot_range` have no production caller
  and the compile path calls `lower_inner`.

So the producer the interning was built for is not merely un-typed; it is a
caller chain nobody publishes, behind a defect nobody has fixed, on both
backends. Whatever eventually publishes caller scopes will decide for itself what
shape the metadata wants — and a guess at that shape, held only by its own tests
on the soundness-critical materialization path, is the wrong thing to review on
every future deopt-metadata change.

### 7.4 What survives the deletion, so redoing it is not from zero

* **The cost is still reported.** `metrics.rs::frame_state_heap_bytes` totals the
  owned representation per artifact (`method_key.len()` + slots × 56 B, per point,
  recursing through `caller`). The motivation therefore survives, measurably, even
  though the implementation does not — which is the test a deletion of an
  optimisation has to pass.
* **This document.** §2 is a complete description of a design that worked and was
  tested; §4 records what the representation achieves on its ideal input, with the
  caveat attached.
* **The semantics.** `ResumeSemantics` reached `DeoptimizationPoint` and is
  checked; only its *per-caller-scope* form was interned-only, and §5.2's one
  remaining consumer edit (the VM resume sink reading `point.semantics` instead of
  re-deriving from `reason`) is unaffected by the deletion and still open.

### 7.5 What the deletion did not close — *all three closed 2026-09-22*

Round 10 wave 9 (lane `deoptretire`) closed all three. They are kept here, with
their outcomes, because the list is also the retroactive check on §7's decision:
if any of them had needed interning to come back, the deletion would have been
wrong. None did.

1. **§5.2's consumer edit.** ~~`vm/src/runtime/interpreter.rs` still infers
   re-execute-vs-resume from `DeoptReason`.~~ **Done.** Every routing and resume
   decision reads `semantics` — both trampoline entries, the OSR
   exception-exit provenance check, both rebuild sinks, and the new
   `deopt_resume::scope_resume_pc`. `reason` is the charging key and nothing
   else. See §5.2 for the list and for the one thing deliberately left refused
   (honouring a `RESUME` point in the ordinary sink, which the stash's nesting
   argument forbids).
2. **Per-caller-scope semantics.** ~~The owned `FrameState` has no semantics
   field, so only the innermost scope's survive on a point.~~ **Answered at the
   unwind.** `ReconstructedFrame` carries `semantics` per frame:
   `reconstruct_frame` and `reconstruct_frame_from_machine_state` stamp the
   trapping frame with the point's and every linked caller with
   `ResumeSemantics::for_caller_scope()` — which is what the interned form did
   per scope, at the one place the chain is walked. The metadata still cannot
   say a caller scope is anything BUT `RESUME`; nothing in either backend can
   produce such a scope, and the field's doc in `jit/src/deopt.rs` says so.
3. **`ir::InlineScopeTable`.** ~~Still unpopulated, and now the largest
   remaining piece of staged-but-unreached deopt machinery in the tree.~~
   **Retired**, option B of
   `docs/feature-designs/jit-r10-interner-proposals.md` §2, together with
   `ir_lower::Lowerer::caller_chain_for`. It was the worse of the two staged
   pieces, not the better: the interner had no production reference at all, so a
   `rg` told the truth about it, while this table's READ side was on the
   production lowering path and a `rg` reported inlined-scope support this tier
   has never had. §5.1 records what it was and what a re-landing owes.
