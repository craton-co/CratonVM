# JIT round 10, wave 7, lane `report` — proposals

**Written:** 2026-09-21. **Lane:** `report` (wave 7 of the round-10 JIT review).
**Owns:** `vm/src/jit/code_cache_lifecycle.rs`, `vm-cli/src/main.rs`,
`vm/tests/no_test_only_public_api.rs`.

This lane closed
`docs/internal/retired/r10-wrappers-process-report-has-no-reader-and-seven-unfed-fields-20260921-RETIRED-20260922.md`
in the order that page insisted on: suppress the unsourced fields first, then
add the print site. What it could **not** do is give those fields a source,
because every one of them would have to be produced inside `cratonvm-jit`, and
`jit/src/lib.rs` belongs to other lanes this wave.

Everything below is a JIT-side edit specified precisely enough to be made
without re-deriving it, with the VM-side line that consumes it. Each proposal is
independent: `CodeCacheLifecycleRaw::modelled_sources` is four separate flags
for exactly this reason, so any one of these can land alone and turn exactly one
`n/a` back into a figure.

**Read, not executed.** This lane may not build, test or run the regression
suite. Every claim below is from the source text of `jit/src/lib.rs`,
`jit/src/tiered.rs` and `vm/src/jit/code_cache_lifecycle.rs` at `3010c49fa`
plus this lane's own edits. Line numbers are from that tree.

---

## The shape of the VM side, once, so the four proposals can be short

`vm/src/jit/code_cache_lifecycle.rs`'s `code_cache_lifecycle_raw()` assigns
`raw.modelled_sources = ModelledFieldSources::NONE;` near the top. Each
proposal below adds a pull beside the existing ones and flips one flag:

```rust
// e.g. proposal 1
let peak = cratonvm_jit::jit_code_peak_live_bytes();
raw.peak_live_bytes = peak;
raw.modelled_sources.peak_live_bytes = true;
```

Nothing else changes. `Display` already has the `if` on each flag, with the
figure-printing arm written and reachable (an instance of the model takes it
today, and `an_instance_report_prints_the_modelled_groups_it_does_feed` pins
it), so no formatting work comes with any of these.

---

## Proposal 1 — `peak_live_bytes`: a high-water mark on the published-bytes counter

**Cost: one `fetch_max` on a path that already does two `fetch_add`s. This is
the cheapest of the four by a wide margin and should land first.**

`RECLAMATION` in `jit/src/lib.rs` already maintains `installed_bytes` and
`reclaimed_bytes`, both counted on the executable buffer of a *published* body
(`ExecutableBuffer::mark_published` and its `Drop`), so
`installed_bytes - reclaimed_bytes` is exactly the published code still mapped —
`JitCodeReclamationStats`' own doc comment says so. That difference is the
quantity a peak is wanted for.

Add a sibling counter and raise it where `installed_bytes` is bumped:

```rust
/// High-water mark of `installed_bytes - reclaimed_bytes`, sampled at each
/// publish.
///
/// A publish is the only event that can RAISE live bytes, so sampling there
/// misses no maximum. The sample is not atomic with respect to the two
/// counters it reads, so a concurrent release can make it low by one body;
/// it can never make it high, which is the correct error direction for a
/// high-water mark an operator reads as "at least this full".
peak_live_bytes: AtomicU64,
```

and at the `mark_published` site, after the `installed_bytes` add:

```rust
let live = c.installed_bytes.load(Relaxed) - c.reclaimed_bytes.load(Relaxed);
c.peak_live_bytes.fetch_max(live, Relaxed);
```

Expose it on `JitCodeReclamationStats` as `pub peak_live_bytes: u64` and load it
in `jit_code_reclamation_stats()` beside the rest.

**Note the subtraction order matters and `saturating_sub` is wanted**, not `-`:
the two loads are independent, so a release that lands between them can make
`reclaimed_bytes` exceed the `installed_bytes` already read, and in release
builds a wrapping subtraction would produce a value near `u64::MAX` and pin the
high-water mark there permanently. That is a worse failure than the counter not
existing, and it is the only hazard in this proposal.

**VM side:** as in the template above.

---

## Proposal 2 — the deferral age, which the queue already knows

**This is the one the module's own §1.4 asks for by name**, and it is worth
placing above the compilation census for that reason. §1.4 states that a
permanently wedged entry chain — the failure `prune_returned_jit_entries`
self-heals — "shows up as a `deferred_bytes` that never falls and a
`max_deferral_sweeps` that climbs, instead of as silent unbounded growth."
Today the process report has the first half and not the second, so the
diagnostic §1.4 promises is half-built.

The queue does not need new bookkeeping. `DeferredOwner` (`jit/src/lib.rs`
around line 16220) already carries:

```rust
struct DeferredOwner {
    /// Retirement generation stamped AFTER the owner's raw targets were
    /// unpublished. ...
    retired_gen: u64,
    ...
}
```

and `JIT_RETIRE_GENERATION` (around 16420) is the current stamp, advanced by
`bump_retire_generation()` on every withdrawal. So the age of the oldest queued
owner, in retirement generations, is
`JIT_RETIRE_GENERATION - min(retired_gen over the queue)`.

Add an accessor that takes the queue lock and answers it:

```rust
/// Age of the oldest owner in the retirement queue, in retirement
/// generations, or `None` when the queue is empty.
///
/// Takes the retirement-queue lock. Called once per process report, never on
/// a hot path. It must NOT be called while holding `jit_threads()` — the lock
/// order `drain_deferred_jit_owners` establishes is queue-before-threads, and
/// this takes only the queue, so it cannot invert anything.
pub fn jit_oldest_retirement_age() -> Option<u64> {
    let q = deferred_jit_owners().lock();
    let oldest = q.iter().map(|o| o.retired_gen).min()?;
    Some(
        JIT_RETIRE_GENERATION
            .load(std::sync::atomic::Ordering::Acquire)
            .saturating_sub(oldest),
    )
}
```

### The unit is NOT sweeps, and the VM side must not pretend it is

`CodeCacheLifecycleRaw::max_deferral_sweeps` is documented as "most **sweeps**
survived by any one queued body", and the model counts exactly that: its
`sweep()` bumps a per-body `deferrals` on every sweep that retains. A retirement
generation is a different unit — it advances once per *withdrawal*, not once per
*drain* — so a queue holding one body while a hundred other methods are
recompiled shows an age of ~100 having survived possibly one drain.

Both units answer "has this been stuck a long time?" and neither is wrong, but
they are not interchangeable, and quietly assigning generations into a field
named `_sweeps` is the kind of relabelling the `const _` numbering assertion in
`code_cache_lifecycle.rs` exists to prevent one level down. Two honest options:

1. **Preferred** — add a distinct raw field, `oldest_deferral_generations: u64`,
   pull into that, and have `Display`'s RETAINED branch print
   `oldest deferred {n} retirement generations`. `max_deferral_sweeps` then
   stays a model-only field and the `deferral_age` flag gates both.
2. Count drains per queued owner in the JIT — a `u32` bumped on every
   `drain_deferred_jit_owners` pass that retains an entry — which matches the
   existing field exactly at the cost of a write per queued owner per drain.

Option 1 costs nothing on the hot path and answers the operational question.
Option 2 is only worth it if someone wants the two designs' numbers comparable.

**`SweepOutcome::oldest_deferral_sweeps` gets the same treatment**: it is
`Option<u32>` as of wave 7 and `sweep_if_quiescent()` answers `None`. With
option 1 it should become a separate `Option` field in generations rather than
have generations poured into the `_sweeps` one.

---

## Proposal 3 — `retirements_by_reason`: the expensive one, and why

`retire_withdrawn_body` counts `withdrawn_bodies` / `withdrawn_bytes` and
nothing else. The callers — `JitCache::put`, `put_osr`, `invalidate_matching`,
`clear_all`, `sweep_cold_bodies`, and the inline-cache withdrawal paths — each
*know* why they are withdrawing, and none of them says.

This is the only one of the four that cannot be done with a counter: it needs a
`reason` parameter threaded through every withdrawal site, which is a change to
call sites in `jit/src/lib.rs` rather than an addition beside them. The mapping
onto `code_cache_lifecycle.rs`'s existing six-code `retire_reason` numbering is
mostly obvious (`put`/`put_osr` → `SUPERSEDED`, `invalidate_matching` →
`INVALIDATED`, `sweep_cold_bodies` → `CACHE_PRESSURE`, `clear_all` →
`SHUTDOWN`), but two are genuinely ambiguous and a wrong guess makes the
histogram worse than its absence:

* `invalidate_matching` serves both assumption invalidation and class
  unloading, which are `INVALIDATED` and `CLASS_UNLOADED` — and telling
  cache-pressure eviction from class unloading is the single reading the
  known-issues page says an operator wants this line for. Folding them defeats
  the purpose.
* `invalidate_matching`'s transitive reverse closure over baked direct calls
  withdraws bodies for a reason that is neither its own: a caller whose callee
  was invalidated. That is arguably a seventh code.

**If this lands, the `const _` numbering assertion in
`code_cache_lifecycle.rs` must be extended to cover the new positional array**,
exactly as it already does for `alloc_failure`. The existing assertion's own doc
comment explains why: the histogram is copied *positionally*, so a divergence
would not fail to compile — it would silently relabel every bucket and print
`class-unloaded` over a count of supersedes. That assertion is three lines and
is not optional.

Until then the report prints
`retirement by reason: n/a for all N retirements (no producer: the JIT's
withdrawal sites do not record WHY ...)`, which is honest and greppable.

---

## Proposal 4 — the compilation census, and one trap to avoid

> **LANDED 2026-09-22 (wave 9). The trap was not walked into, and the route this
> proposal recommends was not taken either** — `MethodState` exists only for
> methods the tiered manager tracks and `DIAG_CORE` sees only the first
> `CompilerCore` this process built, so a census over them can sit below
> `installed_bodies`, which is the same over-estimate by a different road. The
> producer counts publications where publications happen: a per-method-key table
> bumped at `JitCache::put`/`put_osr`, gated on the `mark_published` call that
> bumps `installed_bodies`, so the two count one population by construction. See
> `docs/feature-designs/jit-r10-producers-proposals.md` §4 for the full account.


`recompilations`, `methods_compiled` and `max_versions_for_one_method` need a
per-method version table, which `CodeCacheLifecycle` has and `cratonvm_jit` does
not. `TieredCompilationManager` keeps `MethodState` per method and could answer
all three, but it is per-manager, and `code_cache_lifecycle_raw()` is a
process-wide free function with no manager in hand.

There is a process-global handle: `DIAG_CORE`, a private
`static OnceLock<Arc<CompilerCore>>` at `jit/src/tiered.rs:2634`, set at 3349
and used by `dump_method_stats_to_stderr` for exactly this reason ("so a run
with no tiered manager still reports them"). A free accessor over it is the
route:

```rust
/// `(distinct methods with at least one published body, installs that
/// superseded an earlier body of the same method, highest version reached by
/// any one method)`. `None` when no tiered manager has been created.
pub fn jit_compilation_census() -> Option<(u64, u64, u64)>
```

It would need `MethodState` to carry a version count, which it does not today —
`current_tier` only advances and `note_body_withdrawn` demotes, but nothing
counts publishes per method.

### The trap: `nominate_to_first_body` is NOT `methods_compiled`

`CompilationStats::nominate_to_first_body` (`jit/src/tiered.rs:3264`) reads as
an exact fit — its doc says "Methods whose first method-entry body published
after a request admitted through this manager" — and it is already an
`AtomicU64` on a struct a `stats()` accessor returns. Using it would be wrong,
and quietly so.

Read its only increment site, `note_time_to_tier` around line 1946:

```rust
if state.first_body_ms == 0 {
    state.first_body_ms = now;
    if state.first_nominated_ms != 0 && state.first_nominated_ms <= now {
        ...
        self.stats.nominate_to_first_body.fetch_add(1, Ordering::Relaxed);
    }
}
```

The increment is inside a second `if`. A method whose `first_nominated_ms` was
never stamped, or was stamped on a clock this comparison rejects (the doc of the
sibling `nominate_to_first_body_ms_total` notes that "a request whose stamp is
not on `process_uptime_ms`'s clock — the VM's deopt re-queue passes wall-clock
milliseconds — contributes 0"), gets a first body and is **not** counted. The
counter is a *lower bound* on distinct methods compiled, and it exists as the
denominator of a time-to-tier average, not as a census.

Pouring it into `methods_compiled` would therefore make
`versions_per_method = installs / lower_bound` an over-estimate of unknown size
— and if no stamp ever lands, it is `installs / 0`, which is the `0.0000` this
whole change exists to stop printing. The impossible reading would be back,
wearing a producer.

**So: do not wire proposal 4 from `nominate_to_first_body`.** If the honest
counter is too invasive, leaving this group suppressed is the correct outcome;
`n/a` costs nothing and a plausible wrong number costs an afternoon.

---

## Not a proposal: things this lane checked and found already correct

Recorded so the next reader does not re-derive them.

* **`sequence` on the process sweep path.** The known-issues page flags that
  `sweep_if_quiescent` sets `sequence: after.drains` while `Display` labels it
  `sweep #`. On inspection the *value* is right — the Nth drain of the JIT queue
  is the Nth sweep of it — and the only defect is that the field's doc claimed a
  "1-based sweep sequence number" without saying that on this path it is a
  cumulative count including drains other threads performed. Fixed in the doc
  comment; no code change, and no accessor needed.
* **`external_fragmentation` reading `0.0` with the arena off.** Structural, not
  missing: without the arena every body is its own mapping, so there is no free
  list, and `external_fragmentation` answers `0.0` for zero free bytes. Already
  documented in `code_cache_lifecycle_raw`.
* **`deferrals` had no `Display` reader on ANY path**, fed or not — it was
  computed by the model's `sweep`, snapshotted by `raw()`, asserted by one test
  and printed nowhere. It is printed now (in the RETAINED branch, when a
  producer exists), so it is no longer a write-only field even for the model.
* **`live_code_bytes` is not printed directly and that is fine.** It is read in
  production, by `from_raw`, as the numerator of `internal_fragmentation`, which
  the report does print — and it is recoverable from the two printed figures
  (`live_code = live_bytes × (1 − internal_frag)`). It is not an orphan; adding
  it would pad the line without adding a fact.
* **The cap-refusal count is deliberately counted twice** and the two must not
  be reconciled. `note_jit_code_cache_cap_refusal` bumps both
  `JIT_CODE_CACHE_CAP_REFUSALS` (per-compile-door, printed by
  `tiered::dump_method_stats_to_stderr`) and the allocation ledger's
  `CAP_EXCEEDED` bucket (process-wide, printed by this report). That function's
  own comment says "The two counters are NOT redundant" and why. Checked because
  two counters of the same event printed by two lines of one stderr dump looks
  like a defect and is not.

## One defect found by review rather than assignment, and fixed

`Display`'s allocation line printed `failed={} requesting {} bytes`, a flat
total. `JitCodeAllocFailureStats::failed_bytes` documents itself as "a LOWER
BOUND on the memory the process could not get, not a total: cap-gate refusals
contribute zero because they happen before codegen computes a size ... Compare it
against `failures` rather than treating it as a mean." Printed flat beside a
count, it invited exactly the division that doc forbids — and on a cap-exhausted
cache, the case the line exists for, *every* refusal contributes 0 bytes, so the
mean would read 0 on the run where the number matters most. Now printed as
`requesting >=N bytes` with the reason and an explicit "do not divide".
