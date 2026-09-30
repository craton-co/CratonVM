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

`pgo.rs` states its own unwired status in its module doc, and that is still
true. As of **2026-09-16** it is no longer only asserted: `pgo.rs`'s own test
`pgo_is_named_only_by_comments_outside_this_module` walks every `.rs` file under
`jit/src`, blanks whole-line comments, and fails if `pgo::` or `crate::pgo`
survives. The references that exist today are all doc comments —
`jit/src/ir_schedule.rs:119` (`pgo::BranchProfile`) and `jit/src/profile.rs`
lines 294 and 2592 (`pgo::ReceiverTypeProfile`) — plus `jit/src/lib.rs`'s bare
`pub mod pgo;`, which names the module without using it. (The earlier version of
this paragraph claimed `ir_schedule.rs:119` was the *only* mention; it was not,
and asserting a grep result in prose is why this is now a test.)

Every counter in it is permanently zero at runtime. It is kept as a sketch; §5
of `docs/jit/profile-guided-inlining.md` records the intent to delete its policy
half rather than let it become a second, divergent one.

### The recommended `lib.rs` change

`jit/src/lib.rs:142` should read `pub(crate) mod pgo;` rather than
`pub mod pgo;`. The argument, written out in full in a `REVIEW-NOTE` at the top
of `jit/src/pgo.rs` for the owner of `lib.rs` to apply:

* the module has **no callers at all**, in this crate or out of it, so narrowing
  its visibility loses nothing;
* what it buys is that the module stops being part of `cratonvm-jit`'s public
  API, so an out-of-tree consumer cannot bind to a receiver-profile model that
  disagrees with the live one about table capacity and counter width — the row
  above that is a correctness difference, not a performance one;
* and `dead_code` analysis inside this crate starts telling the truth about
  which of these types are reachable.

Deleting the module outright is the other defensible answer. The honest version
of that edit removes the module, the `pub mod` line, this section's right-hand
column, and all three doc-comment references listed above; the ratchet test
enumerates exactly those references, so it is the list to work from. What is
not defensible is leaving `pub mod pgo;` as it stands.

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

### A receiver class id is not a receiver TYPE (round 9)

Every consumer of a profiled (or CHA-derived) class id turns it into the same
machine guard, `CMP DWORD [recv+0], id`, and that word is not a type: an array's
header carries its COMPONENT's class id there (a primitive array carries `0`),
and `ClassId(0)` is also `java/lang/Object` itself. So:

* a guard at a site whose static receiver type admits arrays (`Object`,
  `Cloneable`, `Serializable`) must also test `KIND_TAGS`, or a `Foo[]` receiver
  passes a `Foo` guard. The MIC/PIC cascades always did; the guarded-inline
  chains (`x64/op_invoke.rs`, `x64/inlining.rs`) do since round 9, gated on
  `static_receiver_admits_arrays` so every other site is byte-identical;
* an inline-cache slot must not use `0` as its EMPTY marker, because a class-0
  receiver matches it. The PIC's empty ways start at `EMPTY_WAY_CLASS_ID` since
  round 9; the MIC is still `0`
  (`known-issues/jit/mic-empty-slot-class-zero-window-20260918.md`).

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
revisit these predicates.

*(Corrected 2026-09-16: this paragraph used to end "`InliningPolicy::should_inline`
inherits the fix and refuses a truncated site
(`inlining_policy_refuses_a_truncated_site`)". That type and that test no longer
exist — `InliningPolicy` was the second inlining policy, deleted for the reasons
in §1 — so the sentence described code that was not there. `ReceiverTypeProfile::shape`
is the surviving reader of `is_truncated`.)*

None of this runs: the module is still unwired, and this change does not wire it.

---

## 5a. The deserialiser is hardened even though nothing calls it (2026-09-16)

`ProfileSerializer::deserialize` read every array length as a `u32` straight out
of the input and used it directly: `Vec::with_capacity(n)` on a file-supplied
count, and `pos + len` bounds tests that can wrap on a 32-bit target. A blob
declaring `0xFFFF_FFFF` receiver entries asked the allocator for
4 294 967 295 × 32 bytes — roughly 128 GiB — before reading a single entry, and
58 bytes of input were enough to ask for it. Allocation amplification: a
decompression bomb with no decompression.

It was never reachable, because nothing calls `deserialize`. It is hardened
anyway, and the reason is the one this whole document is about: wiring a
*deserialiser* to a file is a much smaller and much more plausible edit than
wiring the profile model, and the day somebody writes "read the profile back
from disk" is not the day to discover this.

**Two** bounds are now applied to every count, because they do different jobs:

| | what it is | what it catches |
|---|---|---|
| `MAX_*` cap | a policy number per field — `MAX_METHODS` (2²⁰), `MAX_PER_BCI_ENTRIES` (2¹⁶, the JVM's own `code_length < 65536`), `MAX_TYPE_ENTRIES` (= `ReceiverTypeProfile::MAX_ENTRIES`), `MAX_CALLEES`, `MAX_DEOPT_REASONS`, `MAX_STRING_BYTES` (65 535, the `CONSTANT_Utf8` ceiling) | an absurd count, rejected at the four bytes that declare it, before the collection it sizes exists |
| bytes remaining | `(data.len() - pos) / MIN_*_BYTES`, where every element's minimum encoded size is a known constant | the bound that actually *holds*: a blob of *n* bytes cannot contain more than *n*/min elements however generous the cap is. This is what makes the surviving `Vec::with_capacity` safe rather than merely unlikely to hurt |

Every cursor advance is `pos.checked_add(n)`, so a 4 GiB declared length cannot
wrap `pos + len` past `data.len()` and turn a bounds test into a pass. The
preallocation is additionally clamped to `PREALLOC_CLAMP` (1 024), so even a
count that passes both bounds cannot turn one `with_capacity` into a large
up-front reservation.

`MAX_TYPE_ENTRIES` is deliberately the type's own invariant rather than a looser
number. Accepting more would hand the rest of the module a
`ReceiverTypeProfile` that violates the cap `is_truncated()` and `shape()` reason
about (§5) — the deserialiser is the one path into this module that does not go
through `add_receiver` or `merge`, both of which enforce it. If the cap is ever
lowered towards HotSpot's `TypeProfileWidth = 2`, `PGO_VERSION` is what changes
with it.

**What is still not checked**, and is a property of any future consumer rather
than of this function: the *semantics* of a well-formed blob. It may claim
`total_calls = 0` alongside entries summing to a million, or name the same bci
twice (the later record wins, silently), or give a method a `class_name`
matching no loaded class. Since a profile in this VM is a heuristic and never a
correctness input (§3), that is survivable — but a consumer that wires this to a
file is consuming attacker-chosen profile numbers and has to say so itself.

Tests, all in `pgo.rs`'s own module: a blob declaring `u32::MAX` methods, one
declaring `u32::MAX` receiver entries, one whose count is inside the cap but
larger than the bytes behind it, one with a `u32::MAX` string length, **every
prefix** of a blob that exercises each array in the format, and a full
eight-entry receiver table that must still round-trip. The existing round-trip
tests are unchanged and still pass.

---

## 5b. Persistent profiles for the LIVE store (2026-09-16)

§5a hardened a serialiser nothing calls, on the argument that "wiring a
deserialiser to a file is a much smaller and much more plausible edit than
wiring the profile model, and the day somebody does it is not the day to
discover this." Somebody did it. `jit/src/profile_store.rs` writes the **live**
store (`profile.rs`, the left-hand column of §1) at VM shutdown and replays it at
VM startup — the warm-up-elimination shape Azul calls ReadyNow, aimed at the
Spring Boot / Hibernate / Netty workloads this repository benchmarks, where
warm-up dominates.

It shares **no code** with `pgo.rs`. It could not: the two models disagree about
receiver-table capacity and counter width (§1), and a serialiser is a statement
about the model it serialises. What it borrows is the *shape* — magic, version,
every count bounded twice, every cursor advance checked — and it is held to a
higher standard than §5a's, because unlike that one it actually reads a file.

### Identity is by name, and that is the whole correctness story

`profile::MethodKey` is `(class_id, method_name, descriptor)`, and `class_id` is
allocated **per VM and per run**: a dense index handed out in class-load order,
which depends on timing, class path and which lambda proxies got minted first.
A format that stored `class_id` would not merely be *stale*, which is
survivable — it would be *systematically wrong*, seeding `java.util.HashMap`'s
receiver profile into an unrelated Spring bean on essentially every run.

So the file stores strings, for the method key **and** for every receiver class
id inside a receiver profile — that second half is the one that is easy to
forget, because `ReceiverCounts` is `class_id`-keyed for the same reason and is
just as meaningless across processes. `load_into` re-resolves every name against
this run's ids through a caller-supplied resolver; a name that resolves to
nothing is dropped and counted, never guessed at. `cratonvm-jit` cannot do that
resolution itself — it does not depend on the VM crate, and the only class-namer
hook installed today (`vm_init.rs:4664`) runs in the opposite direction, into
`cratonvm-gc` — so the resolver is a closure the VM call site passes in.

A name is not a complete identity either: a class is `(name, defining loader)`,
and a loader id is per-run in exactly the way a class id is, so recording one
would be the same mistake one level up. The rule for a name two loaders have
both defined is therefore to **drop it**, and the resolver the call site passes
has to be the class manager's `find_unique_class_by_name`
(`class_manager.rs:9398`, "succeeds only when one loader has defined the
requested name") rather than the `#[deprecated]` loader-blind
`find_class_by_name`. In a JBoss or Spring Boot deployment with per-module
loaders that is not a corner case, and the honest accounting is that replay does
less there: the ambiguous methods are counted as unresolved and start cold,
exactly as they would have without a file.

### The format

Little-endian. Magic `"CRP1"` (`0x43525031`), version 1, a zero-checked reserved
half-word, a method count, then one **length-delimited** record per method:
names, an invocation count, and four pc-keyed arrays (branches, call sites,
loops, receiver tables). The length delimiter is load-bearing twice: a corrupt
count inside a record cannot reach past that record, so one bad byte costs one
method and the error names it; and it is the hook a future version would use for
skip-forward compatibility, which is why version 1 is strict about a record
being exactly its declared length rather than relaxing that now for a
compatibility nobody needs yet.

Serialisation is deterministic — methods sorted by `(class, name, descriptor)`,
arrays by pc, receiver tables by descending count with ties broken by ascending
class name, the same ranking rule `summarize_receivers` and
`classify_receiver_shape` use. Two saves of one store are byte-identical.

The format caps a receiver table at 4 096 classes where the live store caps
nothing. That is a real loss of the property §1 makes load-bearing, and the cap
is chosen so it cannot cost anything: 4 096 is 512× `INLINE_MEGAMORPHIC_TYPE_CEILING`
(8), so a site truncated by it still arrives with 4 096 types and still
classifies as megamorphic — which refuses. Truncation keeps the highest-count
classes, so what it discards is the tail.

### Replay is a hint, and the rule differs per axis

The replay adds **no new consumer and no new speculation**: §3's table is
unchanged, because nothing in `jit/src/lib.rs` can tell a replayed count from a
live one and nothing is taught to. A replayed receiver type is re-validated by
the same `CMP DWORD [recv+0], guard_class_id` a live seed is re-validated by.

What a stale or adversarial profile **can** cause: a method compiled sooner than
it earned (bounded, below), an inline cache seeded with the wrong class (one
miss), a speculative inline guarded on the wrong class (one deopt, then
re-profiling), a block laid out on the cold path, an unroll factor for a trip
count that no longer happens. What it **cannot** cause: a wrong answer. It can
cause a wrong *speculation*, which deopts; it must not cause a wrong *result*,
and nothing here is permitted to be the first consumer that would make that
false.

`ReplaySeedPolicy` bounds each axis by one question — *is there something
downstream that re-checks this number against reality?*

| axis | re-checked by | seeding rule |
|---|---|---|
| invocation count | **nothing** — it decides whether to compile at all | `min(recorded / 4, gate / 5)`, where `gate` is the smaller of `CRATONVM_JIT_THRESHOLD` and `CRATONVM_TIER_C1_THRESHOLD`, clamped strictly below `gate` |
| receiver counts | the inline cache's own `CMP`, at every dispatch | capped at 1 000 — above the 250-observation speculation floor so replay is useful, low enough that 1 000 contrary live observations outvote it |
| branch counts | nothing, but nothing rests on them | capped at 5 × `BRANCH_BIAS_MIN_SAMPLES` (100), so ~100 contrary live observations flip the hint |
| loop trips | the loop's own trip test | back-edge count capped; `entry_count` and `total_trips` scaled **together**, because the average is what the unroll heuristic reads and it is scale-free |

The invocation row is the one that needed an argument rather than a number.
Because the ceiling is strictly below the gate, a replayed count **cannot on its
own** make any method eligible for any tier: at the default gate of 500 the
method still has to make 400 real calls this run, so the most replay can buy is
20% of the warm-up distance, and proportionally less for a method that was not
hot last run. The rejected alternative was "seed in full and flag it
`from_replay` so the tiering policy can discount it" — a better rule *once the
policy reads the flag*, and a strictly worse one until then, because the flag
has no reader and the full seed does. The provenance bit exists anyway, so that
policy can be written later against real data instead of discovering it has
none.

### Provenance and the census

`profile::ReplayProvenance` hangs off `MethodProfile` (a null pointer for every
method that was never seeded, which is every method in a default run). It
carries the `from_replay` bit, the seeded counts, and an undo log of
`(pc, class)` pairs awaiting a live verdict. `record_receiver` — the one live
receiver path — judges them:

* the live run sees the **same** class at that pc: *confirmed*, the seed stays;
* the live run sees a **different** class there: *refuted*, every still-pending
  seed at that pc is subtracted back out and the method is marked contradicted.
  The live observation that did the refuting stays, so the site re-profiles from
  live evidence alone.

The asymmetry is the point. "My stored evidence disagrees with what I am
watching" is answered by discarding the stored evidence, not by averaging it in.

`profile_store::report_replay_outcome` emits the totals at `info!`, and the
refuted half at `warn!` — never `debug!`/`trace!`, because the workspace
`Cargo.toml` pins `release_max_level_info` and a counter whose only reader is a
`debug!` is invisible on a release binary, which is the only kind of binary that
can answer the question. A run whose refuted count approaches its confirmed
count is a run paying deopt for hints that were wrong.

Known limit, stated rather than papered over: the tallies live inside each
method's profile, so `invalidate_class` takes them with it on class unload and a
census taken afterwards under-reports both halves. The alternative was a pair of
process-global counters, which `jit/tests/process_global_statics_ratchet.rs`
exists to stop this crate growing and which two VMs in one process would share.

### Flags, both default OFF

| variable | effect |
|---|---|
| `CRATONVM_JIT_PROFILE_SAVE=<path>` | write the store at VM shutdown |
| `CRATONVM_JIT_PROFILE_LOAD=<path>` | read it at VM startup |

Both take a **value**, so neither is presence-parsed: they are read with
`flags::runtime_var` and an unset, empty or all-whitespace value means off.
There is no `=1` spelling and no default path.

A missing file is `Ok` with `file_missing` set — the first run of a save/load
pair is "no hints", not an error. Everything else (bad magic, bad version,
truncation, non-UTF-8, a count past either bound) is `Err`, and
`load_if_configured` turns it into a `warn!` and continues with an empty
profile. Failing a VM's startup over a profile file would be the same category
error the whole design forbids: **a bad profile must never be able to change a
program's answer** — including by turning a successful run into a failed one.

Tests, all in `profile_store.rs`'s own module: an identity-policy round trip
compared as an equality; the same profile written under one class-id space and
read under a completely different one, asserting that the producing run's raw
ids appear nowhere; `u32::MAX` method and receiver-type counts; a count inside
its cap but past the bytes behind it, at two nesting levels; **every prefix** of
a well-formed file; a bad magic, a bad version, a non-zero reserved field, a
`u32::MAX` string length, invalid UTF-8, trailing bytes, and a record longer
than its fields; a confirmed receiver keeping its seed while the census moves; a
contradicted one being subtracted back out; the invocation ceiling staying below
the gate at nine different gates including `1` and `u32::MAX`; a replayed branch
hint being outvoted by live traffic; a capped loop seed preserving its average;
and a missing file and an empty profile both reading as "no hints".

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
6. **The `lib.rs` change is recommended, not made** (added 2026-09-16). The
   change that produced §1's "recommended `lib.rs` change" and §5a was not
   permitted to edit `jit/src/lib.rs`, so `pub mod pgo;` is still exactly what
   is there. Until somebody applies `pub(crate)`, the fencing is one test and
   one long doc comment, and neither of them stops an out-of-tree consumer:
   `cratonvm_jit::pgo::…` compiles today, and the ratchet only scans `jit/src`.
   Owner: whoever owns `jit/src/lib.rs`. Next step: apply the one-line change
   written out in the `REVIEW-NOTE` at the top of `jit/src/pgo.rs`, or delete
   the module using the reference list that test produces.
7. **No fuzzing of the hardened deserialiser** (added 2026-09-16). §5a's tests
   are hand-built blobs and an exhaustive prefix walk of one well-formed blob.
   That covers truncation completely and the six declared counts by example; it
   does not cover *arbitrary* byte mutation. `fuzz/` exists in this workspace
   and `deserialize` is a textbook `libfuzzer` target — `fn(&[u8])` returning
   `Result`, no I/O, no global state. That target was not added here because
   this change does not own `fuzz/`, and it is the obvious next step for anyone
   who actually wires the deserialiser to a file. Nothing should read a PGO blob
   from disk before that target exists and has run.
8. **§5b's replay has not been measured, and no warm-up claim is made for it**
   (added 2026-09-16). Not "measured and inconclusive" — *not measured*. No
   benchmark was run, none could be in the session that wrote it, and the
   module's own doc says so in the same words. What can honestly be said is what
   the mechanism *can* do: it puts last run's branch bias, receiver shapes, loop
   trip counts and a bounded fraction of last run's invocation credit into the
   store before the first frame executes. What a default flip would need is the
   interleaved A/B this repository already runs for the nine tracked apps,
   reporting time-to-steady-state with and without `CRATONVM_JIT_PROFILE_LOAD`,
   **plus** the `ReplayOutcome` census from those runs showing the replayed
   shapes were mostly confirmed rather than refuted. A speed-up with a high
   refuted count is a speed-up bought somewhere other than where it is claimed.
9. **§5b's module declaration and call sites are described, not wired** (added
   2026-09-16). The change that produced §5b was not permitted to edit
   `jit/src/lib.rs` or anything under `vm/`, so as it stands `profile_store` is
   not declared as a module and nothing calls `load_if_configured` or
   `save_if_configured` — the feature is unreachable, not merely off. Both
   edits, with their insertion points quoted from the files they go in, are
   written out in the `REVIEW-NOTE` at the top of `jit/src/profile_store.rs`
   (items 1 and 4). The flag surface — `flag_groups.rs`, `flag-surface.txt`,
   `flag-tokens.md`, `flag-inventory.md` — was found already landed and is
   recorded there as verified (item 2); note that it landed *before* the read
   site, so `check-surface.sh` check 5 was red for those two rows until this
   module started reading them.
10. **No fuzzing of §5b's deserialiser either** (added 2026-09-16). Same
    argument as item 7 and a stronger one, because this deserialiser has a
    caller by design. `profile_store::load_into` is `fn(&ProfileStore, &[u8],
    &dyn Fn(&str) -> Option<u32>) -> Result<_, _>`, and its pure half,
    `decode(&[u8])`, is a textbook `libfuzzer` target with no I/O and no global
    state. The tests are an exhaustive prefix walk plus hand-built hostile
    blobs, which covers truncation completely and each declared count by
    example; they do not cover arbitrary byte mutation. `fuzz/` is not owned by
    that change either. **Nobody should flip `CRATONVM_JIT_PROFILE_LOAD` on by
    default before that target exists and has run.**
11. **The replay census is lost on class unload** (added 2026-09-16).
    `ReplayProvenance` lives inside each `MethodProfile`, so
    `ProfileStore::invalidate_class` takes a class's confirmed/refuted tallies
    with it, and a census taken after an unload under-reports both halves. This
    is a deliberate trade against a pair of process-global counters — which
    `jit/tests/process_global_statics_ratchet.rs` exists to stop this crate
    growing, and which two VMs in one process would share — but it does mean
    the numbers item 8 asks for are a lower bound on a run that unloads classes.
