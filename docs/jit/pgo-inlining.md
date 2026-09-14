# PGO inputs for inlining — what the profile actually records

Companion to `docs/jit/profile-guided-inlining.md`. That document specifies the
**policy** (`plan_inline`, `classify_receiver_shape`, budgets, verdicts,
dependencies) which lives in `jit/src/lib.rs`. This one specifies the **data**
the policy reads: `jit/src/profile.rs` (the live store the interpreter feeds)
and `jit/src/pgo.rs` (an unwired design sketch), and states exactly how far each
number can be trusted.

Read it before adding any consumer, because the two modules answer the same
questions with different guarantees, and the difference is a correctness one.

---

## 1. Two profiles, one of which is real

| | `jit/src/profile.rs` | `jit/src/pgo.rs` |
|---|---|---|
| Fed by | the interpreter, `vm/src/runtime/interpreter*.rs` | **nothing** |
| Read by | `jit/src/lib.rs` (branch hints, MIC seeds, `classify_receiver_shape`) | nothing |
| Receiver type table | **uncapped** | capped at `MAX_ENTRIES = 8` |
| Counter width | `u32`, saturating | `u64` |
| Concurrency | many OS threads, per-method `Mutex` | single-threaded by construction |

`pgo.rs` states its own unwired status in its module doc, and that is
still true: `grep -rn 'pgo::'` finds one doc-comment mention in
`jit/src/ir_schedule.rs:119` and nothing else. Every counter in it is
permanently zero at runtime. It is kept as a sketch; §5 of
`docs/jit/profile-guided-inlining.md` records the intent to delete its policy
half rather than let it become a second, divergent one.

Everything in §2–§4 below is about the live store unless it says otherwise.

---

## 2. What the live profile records

`profile::MethodProfile` carries four maps, populated by four different
interpreter hooks with four different coverages, and only while
`profile::is_profiling_enabled()` is true — the process default is **false**;
the tiered manager flips it on (`vm/src/vm/vm_init.rs:3254`).

| Map | Recorded at | Covers |
|---|---|---|
| `branches` | every conditional-branch opcode | all branches |
| `loops` | every back-edge, plus a trip-complete event per loop entry | all loops |
| `receivers` | receiver resolution in `invokevirtual` / `invokeinterface` | **virtual + interface sites only** |
| `call_sites` | `record_call_site` | every invoke kind, where the interpreter calls it |

Two absences are load-bearing and are reported as such rather than as zero:

* `CallSiteEvidence::None` — nothing was recorded at this bci. A static call
  site has no receiver to record, so a consumer that reads "no receiver
  evidence" as "cold" refuses to inline every `invokestatic` in the VM.
  `CallSiteEvidence::Direct` / `Receivers` name which hook answered.
* `MethodProfile::receiver_summary(pc) -> None` — same distinction on the
  receiver side. Not "zero types".

### Receiver-type profiles: completeness

`MethodProfile::record_receiver` inserts **every** distinct class it sees.
There is no `TypeProfileWidth` cap, so the live store cannot lose a type:
`ReceiverProfileSummary::types` is exact, and a one-type reading is one type —
not a full table that overflowed. This is asserted by
`live_receiver_profile_records_every_type_it_sees`.

That property is the reason `classify_receiver_shape` may read a one-type map
as `Monomorphic`. It does **not** hold for `pgo::ReceiverTypeProfile`, and the
two must never be swapped for one another; see §5.

### Receiver-type profiles: fidelity

The live store's incompleteness axis is magnitude, not type count. Counters are
`u32` and **saturate** at `u32::MAX`. `summarize_receivers` reports this as
`ProfileFidelity`:

* `Exact` — no counter and no total has pinned. Shares are the observed shares.
* `Saturated` — a counter or the total has pinned. Every share understates the
  pinned entries and overstates the rest.

Saturation is *not* the same failure as truncation and is much less dangerous:
a pinned counter stops the profile improving, it does not invert it. Before
this change `record_receiver` used a plain `+= 1`, which panicked in debug
builds at 2^32 observations and **wrapped to zero** in release ones — turning
the program's majority receiver into its rarest. Saturating is the fix;
`ProfileFidelity` is how a consumer finds out it happened.

### Ranking is deterministic

`FxHashMap` iteration order is not stable, so anything that picks a "top"
receiver must impose an order. `summarize_receivers` ranks by descending count
with ties broken by **ascending class id** — the same rule
`classify_receiver_shape` uses (`lib.rs:4754`), so the two cannot disagree.
`dominant_receiver` previously used `max_by_key`, which returns whichever tied
entry came last in iteration order; two compilations of the same profile could
seed different inline caches.

### Arithmetic

Every share comparison is computed in `u64`. In `u32`:

* `count * 100` overflows at 42 949 673 observations — seconds of traffic at one
  hot virtual site;
* `taken * 10` overflows at 429 496 730, and `total * 9` at 477 218 589.

Both regimes panicked in debug builds (inside a JIT profile read) and answered
arbitrarily in release ones. `lib.rs:4775` had already been fixed for this;
`profile.rs` had not, and `dominant_receiver` is the function the production MIC
seed calls (`lib.rs:12803`, `lib.rs:14408`).

---

## 3. Concurrency: what is guaranteed, and which uses may rely on it

These counters are written by many real OS threads and read by a compiler thread
holding none of their locks.

**Guaranteed.** Within one method's profile, a read is a point-in-time
consistent image. `ProfileStore::get_profile` and `snapshot_all` clone the four
maps while holding that method's own `parking_lot::Mutex`, and every recorder
takes the same mutex. So:

* no torn read and no half-applied increment — a summary's total always equals
  the sum of its own parts;
* no lost updates — the increments are read-modify-writes on hash-map entries,
  which an atomic counter would not have made safe;
* successive snapshots of one method are monotone non-decreasing.

`concurrent_receiver_recording_is_lossless_and_monotone` asserts all three under
four concurrent recorders. It has no timing assumption: its polling loop is
allowed to observe nothing at all, so it cannot flake either way.

**Not guaranteed.** Anything crossing a method boundary. `snapshot_all` walks
shard by shard, and `snapshot_invocation_counts` reads `Relaxed` atomics, so two
methods in one snapshot may be from different instants. Freshness is never
guaranteed — the profile is a lagging image at every read — and
`invalidate_class` can drop a method's history entirely when its class unloads.

**The rule this implies.** A profile read is sound as a **heuristic** and never
as a **correctness input**:

| Use | Class | Why it is safe |
|---|---|---|
| Branch layout from `is_usually_taken` | heuristic | wrong answer costs a mis-laid-out branch |
| MIC/PIC seed from `dominant_receiver` (`lib.rs:12803`, `lib.rs:14408`) | heuristic | the cache's own `CMP` re-checks the class id at every dispatch; a stale seed costs one miss |
| Unroll factor from `LoopTripProfile` | heuristic | the loop's own trip test still runs |
| Ranking inline candidates (`hot_call_sites`) | heuristic | ordering only |
| **Choosing which guard to emit** at a speculative site | heuristic | correctness rests on the guard, not the profile |
| **Eliding a check** because the profile says it always passes | **would be a correctness input — not done, and must not be** | nothing re-checks it |

Every speculative decision in `plan_inline` is in the fifth row: the profile
selects a `guard_class_id`, and an exact `CMP DWORD [recv+0], guard_class_id`
routes every other receiver to normal dispatch. A stale or saturated profile
therefore costs performance and never correctness. The deopt/guard pairing for
those verdicts is specified in `docs/jit/profile-guided-inlining.md` §3 and §5;
this change adds no new speculation and enables nothing.

---

## 4. Budget constants live in one place

The depth limit, callee size limits, expansion caps, per-method budget and
recursion cut are **not** in `profile.rs` or `pgo.rs`. They are named constants
in `jit/src/lib.rs` with their rationale attached, tabulated in
`docs/jit/profile-guided-inlining.md` §2: `INLINE_MAX_DEPTH` (9),
`INLINE_MAX_RECURSIVE_DEPTH` (1, counting **ancestors** on the inline stack,
not sibling copies), `MAX_INLINE_SIZE_COLD` (35), `MAX_INLINE_BYTECODE_SIZE`
(325), `MAX_INLINE_EXPANSION_COST[_HOT]` (64/512), `MAX_INLINE_BUDGET[_HOT]`
(750/2000), `INLINE_MIN_SPECULATION_OBSERVATIONS` (250),
`INLINE_MONOMORPHIC_SHARE_PCT` (90), `INLINE_BIMORPHIC_SHARE_PCT` (92),
`INLINE_MEGAMORPHIC_TYPE_CEILING` (8).

They are deliberately **not** restated here. Two copies of a threshold is how a
policy and its data source come to disagree, and the constants belong next to
the function that applies them. `profile.rs` names only the one threshold it
applies itself, `BRANCH_BIAS_MIN_SAMPLES` (20).

---

## 5. `pgo.rs`: the truncation the sketch really does have

`pgo::ReceiverTypeProfile` records at most `MAX_ENTRIES = 8` classes. Once the
table is full, a call with a new class still bumps `total_calls` but records
nothing. A site that overflowed is **not** a description of its call site: the
classes it does not name may collectively outweigh the ones it does.

The accounting is exact and cap-independent:

```text
recorded_calls()   = Σ entry.count
unrecorded_calls() = total_calls - recorded_calls()
is_truncated()     = unrecorded_calls() > 0
```

Deriving truncation from the call accounting rather than from
`entries.len() == MAX_ENTRIES` matters twice: it stays correct if the cap
changes, and it also catches the loss `PgoRepository::merge` introduces when it
folds a second repository's entries into an already-full table
(`merge_induced_entry_loss_is_reported_as_truncation`).

The shape predicates now fail closed:

* `is_monomorphic()` / `is_bimorphic()` — require `!is_truncated()`.
* `is_megamorphic()` — true when truncated. Truncation implies at least one
  more type than could be recorded, so a truncated site is at least as
  polymorphic as it looks. Erring towards megamorphic costs a devirtualisation
  opportunity; erring the other way costs a wrong speculation.
* `dominant_type()` — was already safe by construction, because the share is
  measured against `total_calls`, which counts the dropped observations too. It
  now computes that share from the counts rather than from the cached
  `TypeProfileEntry::ratio` field, which only `add_receiver` refreshes and which
  is stale on any profile assembled by hand, by deserialisation, or by a partial
  merge.

**Honest scope.** At today's `MAX_ENTRIES = 8`, a one- or two-entry table cannot
itself have overflowed, so the added clauses on `is_monomorphic` /
`is_bimorphic` are no-ops, and a truncated table already has 8 entries, which
`is_megamorphic`'s `> 4` test already caught. The change is worth making anyway:
it becomes load-bearing the moment the cap is lowered towards HotSpot's
`TypeProfileWidth = 2`, which is exactly the edit whose author would not think to
revisit these predicates. `InliningPolicy::should_inline` inherits the fix and
refuses a truncated site (`inlining_policy_refuses_a_truncated_site`).

None of this runs: the module is still unwired, and this change does not wire it.

---

## 6. What remains unvalidated

1. **No measurement.** Everything here is asserted by unit tests against
   hand-built and thread-driven profiles. Nothing in this change has been run
   against a real workload, and no benchmark separates it from noise. The
   overflow fixes are only observable past 43 M observations at one site, which
   no test suite reaches; the tests reach that regime by seeding counters
   directly.
2. **No new flag, because there is no new behaviour to gate.** The changes are
   an overflow fix (the previous behaviour was a debug panic or a wrapped
   answer), a determinism fix (the previous tie-break was hash order), and
   additive read-only APIs that no production path calls. A declared flag would
   have to be added to `types/src/flag_groups.rs`, which this change does not
   own; none is needed. Nothing here is enabled by default because nothing here
   is enabled at all.
3. **`ProfileFidelity` has no consumer.** `classify_receiver_shape` does not yet
   consult it, so a saturated profile is still classified as if exact. That is
   survivable — saturation degrades a guard's hit rate, not its correctness —
   but a site that has genuinely pinned a counter is a site whose shares are
   meaningless, and the natural follow-up is for `classify_receiver_shape` to
   return `Cold`/`Megamorphic` for a saturated profile rather than trusting it.
   That edit is in `jit/src/lib.rs` and is not made here.
4. **`call_sites` is never populated in a real run.** Verified: `record_call_site`
   and `record_call_site_borrowed` have **no caller** anywhere in `vm/` or
   `jit/` outside `profile.rs`'s own tests. `MethodProfile::call_sites` is the
   only kind-agnostic per-bci counter, so today `call_site_count` can only ever
   answer `Receivers(..)` (virtual/interface sites) or `None`, and
   `CallSiteEvidence::Direct` is unreachable outside tests. The consequence:
   there is still no per-call-site hotness evidence for `invokestatic` /
   `invokespecial` — the exact gap `CallSiteEvidence` was built to report rather
   than paper over. Closing it is an interpreter edit
   (`vm/src/runtime/interpreter/invoke.rs`), not one this change owns.
5. **Saturation of `BranchCounts` is reported but not acted on.**
   `is_saturated()` exists; `is_usually_taken` / `is_usually_not_taken` still
   answer from a pinned profile. For a layout hint that is the right trade, but
   it is a choice, not an oversight.
