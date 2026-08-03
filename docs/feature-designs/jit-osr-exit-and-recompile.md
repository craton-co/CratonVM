# OSR exit, recompile, and the livelock

**Status: the livelock memo and the visibility gap are closed; the exit-state
differential is the remaining item, and its lever already exists.** Entry has
worked for a long time. Exit is the weaker half — an OSR bail that resumes at
the wrong interpreter state re-runs loop iterations, which is a *wrong-answer*
bug invisible to any test that only checks the method terminates.

Answers the `osr-02` lane of `docs/known-issues/c2/deep-research-vm-c2.md`.
Independent of the entry-metadata contract
(`docs/feature-designs/jit-osr-entry-metadata.md`), which owns the publication
site.

---

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

### 2. Making the exit state checkable — **not done; the lever exists**

The brief wants a test that enters OSR, forces an exit, and compares the resumed
frame against the frame an un-compiled run would have had *at the same iteration
count* — iteration count being the discriminating observable, because re-running
iterations is exactly what a weaker check misses.

The forcing half is already built and declared: `CRATONVM_OSR_EXIT_TEST`
(unconditional bail at the loop header) and `CRATONVM_OSR_EXIT_AFTER=N` (bail at
iteration *N*), both default-off. What is missing is the differential: a probe
that counts iterations and compares the JIT-with-forced-exit arm against
HotSpot and `--nojit`. `probes/OsrDeadLocalProbe.java` is the right shape to
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

`probes/OsrDeadLocalProbe` on the release binary, 2026-08-03:

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
`osr_counts_report_every_event_in_a_fixed_order`. There is no CLI surface that
dumps `MetricsSummary` at exit yet, so the counters have not been read end to
end from a live run.

### 4. `osr_exit_points` is still unchecked against where exits are taken

`CompiledMethod::osr_exit_points` is populated only when `deopt_real_enabled()`,
and nothing compares it with the exits that actually happen. The counters above
make that comparison *possible* — an `osr_exited` count with an empty
`osr_exit_points` is now an observable disagreement — but nothing asserts it.

---

## Implementation steps

1. **Per-pc memo** — done before this lane; see above.
2. **OSR lifecycle counters** — done. Instrumented at four sites in
   `vm/src/runtime/interpreter/invoke.rs`: after `osr_enter_planned` returns
   (counted *after*, so a panic in compiled code is not reported as a
   successful entry), at the `i64::MIN` bail, and at both refusal paths.
3. **The exit differential** — next, and the largest. Drive
   `CRATONVM_OSR_EXIT_AFTER=N` over a loop probe that publishes an
   iteration-count-sensitive accumulator, and compare CratonVM-with-JIT against
   HotSpot and `--nojit`. The accumulator must be sensitive to *how many times
   the loop body ran*, not just to the final value, or it cannot see the defect
   the lane exists for.
4. **Cross-check `osr_exit_points`** against observed exits, once step 3 gives a
   harness that takes exits deliberately.

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

## Effort

Steps 1–2: **done**. Step 3: **M**, and it is the one that addresses the
wrong-answer bug rather than its visibility. Step 4: **S**, gated on step 3.

---

## See also

* `docs/feature-designs/jit-osr-entry-metadata.md` — the entry half, and the
  three coordinate spaces.
* `docs/jit/on-stack-replacement.md`, `docs/jit/osr-vm-side-wiring.md`.
* `docs/jit/loop-rewriter-wiring.md` — the armed/unarmed measurement trap.
* `docs/jit/compilation-broker.md`, `docs/jit/broker-install-epoch.md` — the
  request-identity machinery the brief pointed at for the memo.
