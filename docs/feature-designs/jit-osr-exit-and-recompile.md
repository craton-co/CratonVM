# OSR exit, recompile, and the livelock

**Status:** Shipped (default on), with one dead counter.

## What it does today

Entry is the strong half; exit is the one that can be *wrong* rather than
merely absent, because an OSR bail that resumes at the wrong interpreter state
re-runs loop iterations — a wrong-answer bug that no termination test sees.

- **The per-pc compile memo** is `OSR_ENTRY_REJECTS` in `jit/src/lib.rs`, a
  `(method_hash, entry_pc)` set behind an `RwLock` with
  `is_osr_entry_rejected` / `mark_osr_entry_rejected` /
  `osr_entry_reject_count`. `vm/src/runtime/interpreter.rs` adds a per-pc
  exponential-backoff rejection budget on top.
- **Exit classification** lives in `jit/src/osr_exit.rs`
  (`classify_exit_site`, `resume_image`, `OsrExitSite`). The ambiguity refusal
  runs at *admission*, and its predicate is `semantics` alone — not
  `(semantics, reason)`, because the loop-boundary map and the speculative-BCE
  guard share a header bci in every compiled counted loop, and pairing the two
  refused almost every real OSR site.
- **Counters** are `OSR_EVENTS` in `jit/src/metrics.rs`, ungated: a silent exit
  is otherwise indistinguishable from never having entered.
  `osr_entered`, `osr_exited` and `osr_refused_entry` are recorded from
  `vm/src/runtime/interpreter/jit_bridge.rs`.

## What is not built yet

- **`osr_compile_declined` is declared and never recorded.** It has zero
  recording sites anywhere in the tree, so it always reads 0. Either the
  intended site was lost or it was never written.
- **The in-place OSR-exit transfer** (`CRATONVM_OSR_EXIT_TRANSFER`) is
  default-off; the safe reject path is what runs. The comment in
  `jit_bridge.rs` claiming the gate "was dropped in favour of `can_osr_exit`
  alone" disagrees with `jit/src/lib.rs`, so which path is live at that site is
  **not verified**.

## Goal

Make an OSR exit *visible* and *checkable*, and stop the recompile loop from
re-issuing a request it already knows will fail.

---

## Current state, re-verified

The brief names three items. **One of them was already done, and better than it
asks** — recorded here because sizing the lane from the brief would have meant
rebuilding it.

### 1. The per-pc compile memo — **already done, before this lane**

`jit/src/lib.rs` carries `OSR_ENTRY_REJECTS`, an `FxHashSet<(method_hash,
entry_pc)>` behind an `RwLock`, with `is_osr_entry_rejected` /
`mark_osr_entry_rejected` / `osr_entry_reject_count`. The OSR path checks it
before re-running the pipeline. Its own comment records the cost of not having
it: *256 re-compiles over ten H2 `nioMemLZF:` operations*.

It is also finer-grained than the brief asks for. The refusal sites distinguish
two cases, and only one is memoable:

> Only an ARTIFACT-level verdict may be memoed: it is a pure function of a
> deterministic compile, so it reproduces for every future back edge over this
> pc. A **state-dependent** refusal (a slot's type, the local count, live
> operands) must NOT be memoed — the next trip carries different locals and may
> well be admissible.

`osr_refusal_is_permanent` is that predicate. Memoing the state-dependent case
would have turned a transient refusal into a permanent one, which is a silent
loss of OSR service rather than a livelock — a different bug, in the other
direction.

A neighbouring gate covers the case where the compile produces *nothing*:
`is_jit_bail_listed` (RBC.2), added after *35 923 wasted pipelines* on `Nat.inc`.

### 2. Making the exit state checkable — **DONE**

> `apps/probes/OsrExitDifferentialProbe.java` +
> `regression-suite/perf/osr-exit-differential.sh`. Fifteen arms byte-identical
> to HotSpot, with the forced-exit arms taking real entries and real exits
> (88 entered / 86 exited, all at a true loop boundary), and the historical
> defect **injected and caught**: 200 000 requested, **200 006** executed, while
> the sum — a pure function of `n`, i.e. the weak check — stayed byte-identical.
> Full numbers in the closeout. The paragraph below is the 2026-08-03 statement
> of the gap, kept because it is what the work was sized from.

The brief wants a test that enters OSR, forces an exit, and compares the resumed
frame against the frame an un-compiled run would have had *at the same iteration
count* — iteration count being the discriminating observable, because re-running
iterations is exactly what a weaker check misses.

The forcing half is already built and declared: `CRATONVM_OSR_EXIT_TEST`
(unconditional bail at the loop header) and `CRATONVM_OSR_EXIT_AFTER=N` (bail at
iteration *N*), both default-off. What is missing is the differential: a probe
that counts iterations and compares the JIT-with-forced-exit arm against
HotSpot and `--nojit`. `apps/probes/OsrDeadLocalProbe.java` is the right shape to
copy — an FNV-1a accumulator over everything a mis-seeded entry could disturb,
compared across three arms.

### 3. Counting the exits — **DONE**

`jit/src/metrics.rs` gained `OSR_EVENTS`, modelled directly on
`SCHEDULING_EVENTS`: a closed set, a fixed array of relaxed counters, no
allocation and no initialization order.

| Event | Meaning |
|---|---|
| `osr_entered` | the trampoline ran and control reached compiled code at a back edge |
| `osr_exited` | an entered frame bailed back to the interpreter |
| `osr_refused_entry` | a back edge asked and was refused (no enterable offset, or `validate_osr_entry` rejected the live state) |
| `osr_compile_declined` | the OSR compile produced no artifact at all |

Ungated and always on, and that is the whole point:

> **A silent OSR exit is indistinguishable from never having entered.** Both
> leave the method running in the interpreter with a correct answer and no
> diagnostic. But "never entered" is a tuning question and "entered and left
> immediately, every time" is a livelock, and nothing in a default run could
> tell them apart.

`osr_exited` is meaningless alone; read it against `osr_entered`. The two being
close together *is* the livelock — every entry paying for a trampoline and a
local seed, then leaving. `osr_entered` at zero under a hot loop means requests
are being refused or declined, and the other two rows say which.

They reach `MetricsSummary` (and its `to_json`) beside `bailout_categories` and
`scheduling`, so a sink that already reads those gets these for free.

### What the instrumented sites saw immediately

`apps/probes/OsrDeadLocalProbe` on the release binary, 2026-08-03:

| | |
|---|---|
| OSR entries taken | **125** |
| OSR refusals | **125** |
| …of which memoed as permanent | **0** |
| probe accumulator | `5697627218349681645`, matching HotSpot |

**Half the back-edge trips enter and half are refused, and every refusal is the
state-dependent kind.** That is a correct outcome — a state-dependent refusal
must not be memoed, or a transient "these locals are not admissible" becomes a
permanent loss of OSR for the pc — but nobody could see the ratio before, and a
1:1 entry-to-refusal ratio is precisely the signal `osr_entered` and
`osr_refused_entry` were put beside each other to make legible.

Scope note, so the claim is not read as more than it is: the numbers above come
from the `CRATONVM_DBG=osr,jitc` log lines emitted at the very sites the
counters are instrumented at, which is evidence the sites *execute*. The
counting mechanism itself is covered by
`record_osr_event_increments_its_row_only` and
`osr_counts_report_every_event_in_a_fixed_order`. ~~There is no CLI surface that
dumps `MetricsSummary` at exit yet, so the counters have not been read end to
end from a live run.~~ **Closed**: `dump_method_stats_to_stderr`
(`CRATONVM_DBG_JIT_METHOD_STATS=1`) prints all ten rows, and the differential
harness reads that line and fails the run if a forced-exit arm shows no entries
or no exits.

### 4. `osr_exit_points` — **cross-checked**

> Landed, and it corrected two premises stated below.
>
> **`osr_exit_points` is not the set of loop boundaries.** One emitter writes
> both it and the `invokedynamic` uncommon trap's map, so the classification
> consults the recorded `reason` as well as the set:
> `osr_exit_at_loop_boundary` / `osr_exit_off_loop_boundary` /
> `osr_exit_map_missing` / `osr_exit_bci_unrecorded`. The last two are
> cross-checks between metadata one function writes and must read zero; both did
> on every arm, including every loop-transform arm.
>
> **`deopt_real_enabled()` is default-ON** (`Err(_) => true`), so
> `osr_exit_points`, `can_osr_exit` and the in-place transfer are live
> production paths, not opt-in diagnostics. Several comments in the tree —
> including the paragraph below — describe an opt-in that became an opt-out.

~~`CompiledMethod::osr_exit_points` is populated only when
`deopt_real_enabled()`, and nothing compares it with the exits that actually
happen. The counters above make that comparison *possible* — an `osr_exited`
count with an empty `osr_exit_points` is now an observable disagreement — but
nothing asserts it.~~

---

## The interaction to respect

The bytecode loop rewriter is off by default, and arming it **also disables the
native byte-copy unroller** — they are exact complements. Any OSR measurement
comparing an armed run against an unarmed one is measuring both changes at once.
That trap already cost one long triage; see `docs/jit/loop-rewriter-wiring.md`.

## What to refuse

An OSR exit whose resume bci has more than one possible native image, or none.
Under a loop transform the reverse mapping is one-to-many inside the transformed
region, and picking the wrong image is a wrong-code bug rather than a missed
optimisation.

> **Landed 2026-08-04, with two corrections the sentence above does not
> anticipate.** `jit/src/osr_exit.rs` carries the argument in full.
>
> **Where.** At *admission* (`osr_exit_policy`), tagged
> `osr-entry-ambiguous-exit-image` and memoable, **not** at exit. Refusing at
> exit is not the mirror of refusing at entry: by then the body has committed
> iterations and the caller's only remaining move is the safe reject, which
> re-runs all of them. A refusal added at exit would be a way to *cause* this
> lane's defect, not to prevent it.
>
> **What.** `semantics` alone, not `(semantics, reason)`. Refusing on the reason
> took 10 of CratonBench's 11 OSR refusals, all on `matrixKernel(I)I` at one
> bci where the loop-boundary map and the speculative-BCE range guard sit
> together — the ordinary shape of a compiled counted loop — and it was memoed,
> so the kernel lost OSR for the life of the process. `osr_entered` 507 → 509
> and `osr_refused_entry` 11 → 0 once narrowed. Several native images that
> *agree* are not refused either: that is what a loop transform produces, and
> every consumer that reconstructs a frame finds its point by native offset or
> through the copy's own baked box, never by bci.
>
> Which `semantics` disagreement is reachable is not the obvious one. A `RESUME`
> point anywhere in an artifact is already refused wholesale, so it never
> reaches this check; a `RETHROW` point is explicitly allowed to exist. The
> reachable case is `REEXECUTE` vs `RETHROW` — a `PendingException` point
> sharing a bci with a loop-boundary map.

## Risks

1. **The counters make the livelock visible, not impossible.** They are a
   diagnostic. Step 3 is what would catch the wrong-answer half.
2. **`osr_exited` counts the `i64::MIN` sentinel path.** A method that
   legitimately returns `Long.MIN_VALUE` is disambiguated elsewhere (the
   deopt-pending drain); if that ever regresses, this counter inherits the
   confusion.
3. **Exact-count assertions on a process-global counter are order-sensitive.**
   The two metrics tests hold `METRICS_TEST_LOCK`. A duplicated `#[test]`
   attribute registered one of them twice during development and the two copies
   raced — worth knowing, because the symptom was a one-in-N flake with no
   obvious cause.

## What is left after this lane

* ~~**The differential is a Java-level oracle, not a frame comparator.**~~
  **CLOSED.** `CRATONVM_DBG_OSR_FRAME_TRACE` +
  `regression-suite/perf/osr-frame-differential.sh` diff the resumed frame
  against the un-compiled run's, slot for slot. The item is now closed in the
  brief's own words rather than in substance.
* **`osr_exit_off_loop_boundary` has not been observed non-zero.** The row
  exists because the `invokedynamic` trap can produce one; nothing in this lane
  drove one.
* **The de-speculation reason lookup is still by bci and takes the first.**
  `osr_entry_reason_ambiguous_image` counts how often that pick is arbitrary
  (non-zero on CratonBench). Tolerated because admission makes the sink that
  reads it unreachable for an admitted entry — an argument about reachability,
  not about the lookup being right.

---

## See also

* `docs/feature-designs/jit-osr-entry-metadata.md` — the entry half, and the
  three coordinate spaces.
* `docs/jit/on-stack-replacement.md`, `docs/jit/osr-vm-side-wiring.md`.
* `docs/jit/loop-rewriter-wiring.md` — the armed/unarmed measurement trap.
* `docs/jit/compilation-broker.md`, `docs/jit/broker-install-epoch.md` — the
  request-identity machinery the brief pointed at for the memo.
