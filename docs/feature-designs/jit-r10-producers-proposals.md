# JIT round 10, wave 8, lane `producers` — what landed, what did not, and the proposals that remain

**Written:** 2026-09-22. **Lane:** `producers` (wave 8 of the round-10 JIT
review). **Owned:** `jit/src/lib.rs`, `jit/src/x64/op_object.rs`,
`vm-cli/src/main.rs`, `vm/src/jit/code_cache_lifecycle.rs`,
`vm/tests/no_test_only_public_api.rs`, plus new files under `jit/tests/`,
`docs/known-issues/jit/` and `docs/feature-designs/`.

This lane's assignment was to take `docs/feature-designs/jit-r10-report-proposals.md`
as a STARTING POINT and check each proposal against the code. Three of the four
landed; the fourth was a trap and was left alone. Two of the three that landed had
a factual error in the proposal, and one had a remedy that was necessary but not
sufficient. Those are §1-§4 below. §5 is what was found while reviewing the same
surfaces, and §6 is what is left.

**Read, not executed.** This lane may not build, test or run the regression suite.
Every claim below is from the source text of the files named, plus this lane's own
edits. `rustfmt --check` was run on every edited and added file (a formatter, not
a build); `scripts/check-no-diag-prints.sh` and
`scripts/check-orphan-instruments.sh` were run and both exit 0. The compile and
every `cargo test` belong to the orchestrator.

---

## 1. `peak_live_bytes` — LANDED, with a clamp the proposal did not ask for

**What the proposal said:** one `AtomicU64` beside `RECLAMATION`'s counters, one
`fetch_max` at the `mark_published` site, and `saturating_sub` rather than `-`
because a concurrent release between the two loads could otherwise pin the
high-water mark near `u64::MAX`.

**The hazard is real.** Checked against the counters rather than taken on trust.
`R(t) <= I(t)` holds at every instant, but the two loads happen at `t1 < t2` and
nothing relates `R(t2)` to `I(t1)`. A publish of 50 and a release of 60 inside that
window take `I: 100 → 150` and `R: 90 → 150`, so the subtraction is `100 - 150`. In
a release build a wrapping `-` yields ~`u64::MAX`, `fetch_max` latches it, and the
counter is then not merely wrong but *unfalsifiable* — no later sample can lower
it. `saturating_sub` answers `0` for that window instead and the next publish
corrects it.

**Two things the proposal did not have.**

*The load ORDER is load-bearing and is not interchangeable.* Installed first,
reclaimed second gives an error direction of TOO LOW. Reversed, the subtraction
cannot underflow at all — `I(t2) >= R(t1)` always — but the sample is then too
HIGH, and a high-water mark that overstates itself is the one error direction an
operator cannot detect from the report. So `saturating_sub` is not a substitute
for the order; each covers a different failure.

*A lower-bound peak can still read below `live`, which is symptom one coming back
with a producer.* If the skewed sample was the last one taken and nothing has been
reclaimed since, the pulled peak is below `installed - reclaimed` read from the
same snapshot — and "a high-water mark below the current value" is exactly the
arithmetically impossible reading the whole three-wave story exists to stop
printing. Feeding the counter did not remove that hazard, it converted it from
"no producer" into "a producer with a known bias". The remedy is on the VM side,
where both numbers are in hand:

```rust
let live_now = jit.installed_bytes.saturating_sub(jit.reclaimed_bytes);
raw.peak_live_bytes = jit.peak_live_bytes.max(live_now);
```

`live_now` is a valid lower bound on the peak in its own right (the cache was at
least this full: it is this full now) and it is taken from the same `jit` snapshot
`from_raw` derives `live_bytes` from, so `peak >= live` holds in the printed report
by construction. **Do not remove that clamp on the grounds that the counter is now
fed.** Being fed is what makes it necessary.

**And the sample site is in a file the proposal did not name.**
`ExecutableBuffer::mark_published` lives in `jit/src/exec_memory.rs`, not
`jit/src/lib.rs`. Its only two callers in the crate are `JitCache::put` and
`JitCache::put_osr`, both in `jit/src/lib.rs`, so
`note_jit_published_live_bytes_peak()` is called immediately after each of those —
equivalent, because `mark_published` is synchronous (its install accounting is
complete when it returns) and idempotent (a second call re-samples the same figure
into a `fetch_max` that does nothing).

---

## 2. The deferral age — LANDED, in its own field and its own unit

**What the proposal said:** one accessor over `DeferredOwner::retired_gen` and
`JIT_RETIRE_GENERATION`; the unit is GENERATIONS, not sweeps, so it needs its own
field rather than being poured into `max_deferral_sweeps`. Option 1 (a new raw
field) preferred over option 2 (count drains per queued owner in the JIT).

**Honoured exactly, and the distinction is the whole value of this one.** A
generation advances once per WITHDRAWAL; a sweep once per drain. A queue holding
one wedged body while a hundred unrelated methods are recompiled reports an age of
~100 having survived possibly one drain. Both answer "has this been stuck a long
time?" and neither is wrong, and that is precisely why the wrong label would have
been believed.

**Where the design diverged from the proposal, and why it is better:**

* The proposal said "the `deferral_age` flag gates both". It must not. That flag
  gates `deferrals` AND `max_deferral_sweeps`, and the JIT has a producer for
  neither, so flipping it would have printed two unfed figures alongside the one
  real one. The flag stays `false` on the pull.
* A FIFTH flag in `ModelledFieldSources` was considered and rejected. It would
  have made `ModelledFieldSources::ALL` a lie — an instance of the model feeds no
  generation count — which is this type's own disease one level down. And with
  exactly one field in the group, a separate flag is a second copy of the same bit
  that can disagree with it.
* So: `oldest_deferral_generations: Option<u64>`, where `None` IS the suppression.
  Same shape as `SweepOutcome::oldest_deferral_sweeps`, which already had it in
  the same file. **The rule that falls out, worth carrying forward: a flag per
  printed FRAGMENT, an `Option` per field whose absence is itself a reading.**
  `None` also has to be distinguishable from `0` for a reason unrelated to
  provenance — an age of `0` means "queued by the most recent withdrawal", which
  is a real answer the producer can give.

`SweepOutcome` got the same treatment: a second `Option` field rather than
generations poured into the `_sweeps` one, and its `Display` now has three arms,
each naming its unit in the text.

**Option 2 is not worth building, and that is the recorded decision this page's
parent asked for.** Counting drains per queued owner costs a write per queued owner
per drain, on the path that runs when the cache is under pressure, to make one
number comparable with a model no production path executes. The generation age
answers the operational question (§1.4's "has this been stuck?") at the cost of one
queue walk per process report.

---

## 3. `retirements_by_reason` — LANDED, and it was NOT the expensive one

**What the proposal said:** the only one of the four that cannot be done with a
counter; needs a `reason` threaded through every withdrawal site; two mappings are
"genuinely ambiguous and a wrong guess makes the histogram worse than its
absence"; and the `const _` numbering assertion must be extended.

**The assertion requirement was right and was honoured** — a second `const _` block
in `vm/src/jit/code_cache_lifecycle.rs` pins all six codes, because the histogram
is copied POSITIONALLY and a divergence would silently print `class-unloaded` over
a count of supersedes.

**The cost estimate was wrong, and the reason is instructive.** The proposal
reasoned about `invalidate_matching`, which is one predicate scan serving several
causes, and concluded the cause could not be recovered. It can: the cause lives one
level UP, in which NAMED entry point was called. `invalidate_unloaded_class`,
`invalidate_for_class`, `invalidate_for_class_change`, `remove`, `sweep_cold_bodies`
and `clear_all` are six separate public methods, each of which knows its own
reason. Threading the code as a PARAMETER of `invalidate_matching` /
`invalidate_matching_collecting` — rather than inferring it inside — made the change
mechanical: five `retire_withdrawn_body` call sites, six `invalidate_matching*`
call sites, two in-file test call sites, one new `pub mod retire_reason`, one array
field on `RECLAMATION`, one field on `JitCodeReclamationStats`.

**Two of the proposal's four suggested mappings were wrong when checked.**

* **`clear_all` → `SHUTDOWN` is wrong; it is `INVALIDATED`.** Its two production
  callers are `vm/src/vm/vm_exec.rs`'s `redefineClass` handler and
  `vm/src/vm/vm_init.rs`'s `jit_invalidate_adapter` (a synthetic-stub layout
  change), both of which invalidate a RUNNING VM's cache. Nothing calls it at
  process exit. And `clear_all` is a FULL flush, so on a redefinition-heavy agent
  workload it produces the largest bucket in the histogram — which would have been
  labelled "the VM is going away".
* **`remove` → the proposal did not map it at all**, and its reason is
  `DEOPTIMIZED`: its single production caller is the deopt eviction in
  `vm/src/jit/helpers.rs`, behind `tiered::deopt_evicts_method_body(reason,
  action)`. **That is a property of the CALLER, not of `remove`**, whose own doc
  says only "for invalidation", and `remove` is `pub`. A second caller withdrawing
  for another reason would land silently in this bucket. The remedy if one appears
  is a `reason` parameter on `remove` — it cannot be added pre-emptively, because
  that signature is called from a file outside `cratonvm-jit`. This is written into
  `retire_reason::DEOPTIMIZED`'s doc comment as a standing hazard rather than left
  here.

**`SHUTDOWN` therefore has no site and is documented as RESERVED**, on exactly the
terms `code_alloc_failure::NO_EXTENT_LARGE_ENOUGH` already is: it is never SHOWN
(the report's `retire_reason_breakdown` filters zero buckets before labelling
them), so there is no operator to mislead, and deleting it would renumber every
later code in two crates at once the moment one is appended. That is the
difference between a reserved code and an orphaned instrument, and it is the
distinction that makes keeping it correct rather than lazy.

**The transitive closure carries the TRIGGERING reason.** The proposal wondered
whether a caller withdrawn because its callee was invalidated is "arguably a
seventh code". It is not, because these codes name the CAUSE and not the mechanism:
a caller withdrawn because a class it inlined from was unloaded was withdrawn *by
the class unloading*, and an operator reading `class-unloaded=N` wants N to include
it. A `closure` code would answer "how did this body get on the list", which no
line of the report asks.

---

## 4. The compilation census — LANDED 2026-09-22 (wave 9), and the trap was not walked into

**Update, 2026-09-22.** This proposal is done, and the shape it recommended below
is *not* the shape that was taken — for a reason worth recording, because the
recommendation looked right and had a subtle hole.

The honest producer is not in `jit/src/tiered.rs`. `MethodState` exists only for
methods the tiered manager tracks, so a census over it would have been a census of
the manager's view rather than of publications, and `DIAG_CORE` sees only the FIRST
`CompilerCore` this process built (its own doc says so: "a second VM in the process
is fully functional; it is merely absent from this one exit-time dump"). Either gap
gives a `methods_compiled` that can sit below `installed_bodies` — and `installs /
too-small` is the over-estimate this section spends its length warning about, just
arrived by a different road.

What landed instead counts the event the field is named for, where that event
happens. `CompilationCensusState`, a table on the existing `RECLAMATION` static,
holds one publication count per `compute_jit_key_hash`, bumped at `JitCache::put`
and `JitCache::put_osr` **and gated on `ExecutableBuffer::mark_published` reporting
that it performed the install accounting** — which is the same call that bumps
`installed_bodies`, so the two count one population by construction rather than by
argument. `mark_published` returns `bool` now for exactly that purpose. All three
figures are derived from the one table at read time (`len()`, `sum(v) - len()`,
`values().max()`), so they cannot disagree with each other.

Two decisions the proposal did not have to make, both pinned by tests: an OSR body
is a version of the SAME method (`installs` counts it, so splitting it would put
the denominator above the truth), and the table does not forget a withdrawn method
(a recompile after a class unload is a RECOMPILATION, which is the model's own
definition). The table is bounded at 2^17 methods and `jit_compilation_census()`
answers `None` past that, so a truncated denominator is suppressed rather than
printed — a pinned `methods_compiled` beside a climbing `installs` reads exactly
like the thrash the ratio exists to detect.

`tiered::CompilationStats::nominate_to_first_body` is still not wired to anything.
The original reasoning below is kept because it is why.

---

**Original text:**

`recompilations`, `methods_compiled` and `max_versions_for_one_method` remain
suppressed. `tiered::CompilationStats::nominate_to_first_body` is not wired to
anything.

The reasoning is the previous lane's and it holds: the increment sits inside a
second `if` in `note_time_to_tier`, so a method whose `first_nominated_ms` was
never stamped (or was stamped on a clock the comparison rejects) gets a first body
and is not counted. It is a LOWER BOUND that exists as the denominator of a
time-to-tier average. Pouring it into `methods_compiled` makes
`versions_per_method = installs / lower_bound` an over-estimate of unknown size,
and if no stamp ever lands it is `installs / 0` — the `0.0000` this whole change
exists to stop printing, back again WITH a producer behind it, which is strictly
harder to find than the suppression it would replace.

**What an honest producer would need**, so the next lane does not start from the
trap again: a per-method publish count. `TieredCompilationManager::MethodState`
already exists per method and `DIAG_CORE` already gives a process-global handle to
a `CompilerCore` for exactly this kind of question (`dump_method_stats_to_stderr`
uses it "so a run with no tiered manager still reports them"). What is missing is
that nothing counts publishes per method — `current_tier` only advances and
`note_body_withdrawn` demotes. Adding a `versions: u32` to `MethodState`, bumping
it where a body is published, and answering
`(distinct methods with versions > 0, sum(versions) - distinct, max(versions))`
from a free accessor over `DIAG_CORE` would fill all three fields honestly. That is
a `jit/src/tiered.rs` change and was not this lane's.

**Until then, suppressed is the right answer and is now pinned as such**:
`the_process_report_suppresses_every_field_it_cannot_source` asserts
`!raw.modelled_sources.compilation_census` with the reason in the failure message,
so a lane that wires it has to read why before it can make the test pass.

*(That assertion has since been inverted, in the same commit as the producer. It
now asserts the flag is SET, with a message naming the one legitimate reason it
could be clear — a truncated census table — so the pin still costs a reader who
turns it off an explanation.)*

---

## 5. Found while reviewing the same surfaces

**`unsafe_accessor_census()` had no reader, and the orphan gate was RED on
arrival.** `scripts/check-orphan-instruments.sh` exited 1 at this lane's branch
point with exactly one name, `fn unsafe_accessor_census`, **not on the allowlist**
— so it is a new orphan that arrived after the 2026-09-22 freeze at 105 entries,
not debt. Verified independently of the page that reported it (that page,
`r10-offsetkey-unsafe-accessor-census-has-no-reader-20260921.md`, is not present on
this branch): all five of its counters have production feeders —
`UNSAFE_ACCESSOR_SITES_SP` and `_IR` from two doors in `jit/src/lib.rs`, `_OSR`
from `vm/src/runtime/interpreter/jit_bridge.rs`, and `SERVED`/`DECLINED` from eight
sites in `vm/src/jit/helpers.rs`. Fed-but-unread, so it got a reader in
`vm-cli/src/main.rs` (`[cratonvm] JIT Unsafe accessor binds:`) rather than an
allowlist line. It **must not** be allowlisted: the gate's failure message offers
`--update-allowlist`, and taking that offer here would freeze a working instrument
as permanent debt, which the allowlist's own header forbids in as many words.

Note for the orchestrator's delta check: because this name was never ON the
allowlist, the change does not RETIRE an entry. It takes the gate from red to
green with the count unchanged at 105.

**The splice publisher count in `jit/src/lib.rs` was overstated, in both
directions.** Two sites (the comment above `INLINE_MAX_DEPTH` and
`InlineRefusal::PreciseExceptionFrames`' doc) said round 10 had found FOUR deopt
publishers reachable from the splice walk. Re-derived by grepping
`jit/src/x64/inlining.rs` for CALLS rather than mentions: **two**.
`emit_post_invoke_exception_check` is called five times (lines 1789, 3690, 3988,
4097, 4604) and `emit_precise_null_check_field_store` twice (1638, 3735).
`emit_post_alloc_oom_check`, `emit_precise_array_npe_check` and the reason-11
bounds arm appear in that file only inside comments. So the old list over-counted
by two AND omitted the second real publisher —
`emit_precise_null_check_field_store` (`x64/arrays.rs`, reason 10 via
`build_and_record_deopt_point`), which was in neither site's list. Corrected at
both sites, with the enumeration method stated so the next reader can repeat it.
Lane `bybci`'s count is confirmed.

The interlock is likewise **four** statements, not two: `plan_inline`'s refusal,
`build_single_pass_tables`' `inline_sites.clear()` and
`inline_guard_variants.clear()`, and `try_emit_inline_site`'s own refusal on
`self.precise_exception_frames`. All four verified in place. The fourth is the only
one that still holds if a future edit reaches the emitter with a non-empty plan,
which is worth knowing before anyone relaxes one of the other three.

---

## 6. What remains, with pages

* ~~`r10-report-seven-code-cache-fields-still-have-no-producer`~~ — **RETIRED
  2026-09-22** to `docs/internal/retired/`. The compilation census landed (see §4's
  update); the two sweep-unit deferral counters stay suppressed with the recorded
  decision §2 gives, which is the other half of that page's closing condition.
* `docs/internal/fixed-bugs/r10-producers-ir-tier-instanceof-arm-still-has-no-census-FIXED-20260922.md`
  — the optimizing tier's `instanceof` fast path has no census, and it is the arm a
  hot method actually runs. `jit/src/ir_lower.rs`.
* `docs/known-issues/jit/r10-producers-osr-empty-stack-refusals-is-unreachable-outside-x64-20260922.md`
  — `osr_empty_stack_refusals()` cannot be named outside `jit::x64`, so its
  allowlist entry cannot be retired by adding a reader alone. One line in
  `jit/src/x64.rs`, plus a reader this page carries verbatim.
* `docs/known-issues/jit/r10-producers-isb-counter-reader-must-land-with-its-option-signature-20260922.md`
  — the instruction-stream-barrier counter's reader must land in the same commit as
  the `Option<u64>` signature change, in whichever lane owns
  `jit/src/platform.rs`.
* ~~`r10-readers-orphaned-jit-accessors-20260921.md`~~ — **DONE 2026-09-22, lane
  `readers`**, which could edit `vm/src/runtime/interpreter/jit_bridge.rs`. The one
  call landed exactly as that page specified it, and the undercount it caused was
  demonstrated with a differential build rather than argued: two release binaries
  differing in that single line read `sites_bound=1` and `sites_bound=2` against a
  `tiered.rs` figure of 2, with `served=51,196,270` identical in both — so the
  change is to the instrument and not to the binding.
  `fn note_long_long_value_direct_site` left the orphan allowlist (103 -> 102
  entries) and nothing was added, which is exactly the delta that page predicted
  for its five names. Retired to
  `docs/internal/fixed-bugs/r10-readers-orphaned-jit-accessors-FIXED-20260922.md`.

## 7. Ratchets: what moved and what did not

* **`jit/tests/process_global_statics_ratchet.rs` (818, asserts EQUALITY):
  unchanged.** This diff adds NO `static` line under `jit/src`. The peak, the
  per-reason histogram and the five `instanceof` census counters are all FIELDS —
  the first two directly on `ReclamationCounters`, the five on a
  `TypecheckSiteCensus` that is itself one field of it. That is wave 5's remedy and
  the one the `instanceof` census page names. The cost is that `ReclamationCounters`
  now hosts a third ledger that has nothing to do with the code cache, which is
  written into its doc comment rather than left for a reader to notice.
  *(Wave 9's compilation census is the fourth such ledger, for the same reason and
  by the same remedy: a `OnceLock<Mutex<CompilationCensusState>>` field, so the
  ratchet still reads 818.)*
* **`vm/tests/no_test_only_public_api.rs` (`BASELINE_OFFENDERS = 290`):
  unchanged**, and this was checked by list rather than by count — see that file's
  own new entry for the method and the result.
* **`scripts/baselines/orphan-instruments-allowlist.txt` (105): unchanged, 0
  removed and 0 added.** Both accessors this lane added are C2-shaped
  (`jit_oldest_retirement_age` returns `Option<u64>`;
  `instanceof_inline_sites` returns a tuple of integers; both have atomic bodies)
  and both gained a cross-file caller in the same commit, so neither enters the
  census as an orphan. The five `note_instanceof_*` notifiers are `pub(crate)`,
  which the C1 pattern (`^[[:space:]]*pub fn (record|note)_`) does not match, and
  `note_jit_published_live_bytes_peak` is private. The one name the gate flagged on
  arrival (`fn unsafe_accessor_census`) was never on the list, so fixing it removes
  nothing — see §5.
