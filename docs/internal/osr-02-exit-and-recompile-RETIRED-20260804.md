# OSR-02 — retired 2026-08-04: the exit state is checkable, and it was checked

Branches `fix/c2-osr02-exit-differential-20260804` and — for §8, the literal
frame comparison — `fix/c2-osr02-frame-comparator-20260804`. Both merged to
`dev` and pushed.
Retires `docs/known-issues/c2/archive/osr-02-exit-and-recompile.md` and closes
`docs/feature-designs/jit-osr-exit-and-recompile.md`.

The lane's first two items landed 2026-08-03 (`4bec5efca`) and its own closeout
recorded what did not: *"the exit-state differential, which is the increment the
doc called the point of the lane"*, plus step 4's cross-check of
`osr_exit_points` against where exits are actually taken, plus the brief's
"what to refuse". All three are now landed, and the differential **caught a
deliberately injected instance of the exact defect the lane exists for**.

---

## 1. What the brief asked for, and what happened to each item

| Item | Outcome |
|---|---|
| 1 — a per-pc compile memo so a bailed OSR request is not re-issued | **Already done before the lane** (`OSR_ENTRY_REJECTS`, finer-grained than asked: only an *artifact-level* verdict may be memoed). Unchanged here. |
| 3 — count the exits | **Landed 2026-08-03.** Four ungated rows. |
| 2 — **make the exit state checkable** | **Landed here.** `probes/OsrExitDifferentialProbe.java` + `regression-suite/perf/osr-exit-differential.sh`. §2. |
| 4 — cross-check `osr_exit_points` against observed exits | **Landed here**, and it needed a correction the brief did not anticipate. §3. |
| "What to refuse": an exit whose resume bci has more than one native image, or none | **Landed here**, at *admission* rather than at exit, and **narrowed by a measurement**. §4. |
| The counters "have not been read end to end from a live run" | **Closed.** §5. |
| 2, taken **literally** — the resumed frame diffed against the un-compiled run's, slot for slot | **Landed 2026-08-04** on `fix/c2-osr02-frame-comparator-20260804`, closing §6's first residual. §8. |

---

## 2. The differential, and the injection that proves it is not vacuous

### The observables

Every shape in the probe computes the right answer in the interpreter, so
"byte-identical to HotSpot" is a claim about the OSR exit path only if that path
ran *and* the observable can see a replay. Three per shape:

| | |
|---|---|
| `execs` | a static counter incremented once per loop-**body execution**. A re-run iteration increments it twice; a skipped one, not at all. Both the interpreter and compiled code commit the store, so it counts executions rather than iterations *intended*. |
| `trace` | an FNV-1a chain over the induction variable **and the loop-carried state**, mixed once per execution — a digest of the frame at every iteration boundary. This is as close as a Java-level probe gets to the brief's *"compare the resumed frame against the frame an un-compiled run would have had at the same iteration count"*. |
| `result` | the shape's return value: the ordinary correctness check, kept because a transfer that corrupts a live-across local shows up here and nowhere else. |

Eight shapes: a plain `int` sum, a loop-carried dependence the induction
variable cannot re-derive, a per-iteration heap store, live-across locals of
three kinds, a `double` accumulator, nested loops, a `long` induction variable,
and a loop containing an `invokedynamic` concatenation.

### The arms

HotSpot (**the control** — a CratonVM arm compared only against another CratonVM
arm proves nothing about either), `--nojit`, default, `CRATONVM_OSR_EXIT_TEST`
(bail at iteration 0, where reject and transfer coincide), and
`CRATONVM_OSR_EXIT_AFTER=N` for N ∈ {1, 3, 7, 64, 1000} — the arms where the
body **commits** iterations before the exit, which is the whole point.
`--loop-xform` adds the same set with the bytecode loop rewriter armed, which is
where the reverse mapping is one-to-many inside the transformed region.

The rewriter is off by default and stays off by default here: arming it *also*
disables the native byte-copy unroller — they are exact complements — so an
armed arm compared against an unarmed one measures both changes at once
(`docs/jit/loop-rewriter-wiring.md`).

### The result, `n = 300 000`, 2026-08-04

Fifteen arms, all byte-identical to HotSpot, with the OSR path demonstrably
exercised:

| arm | entered | exited | exit sites (boundary/off/missing/unrecorded) |
|---|---:|---:|---|
| `nojit` | 0 | 0 | 0/0/0/0 |
| `default` | 16 | 0 | 0/0/0/0 |
| `exit-test` | 88 | 86 | **86**/0/0/0 |
| `exit-after-{1,3,7,64}` | 88 | 86 | **86**/0/0/0 |
| `exit-after-1000` | 79 | 76 | **76**/0/0/0 |
| `loop-xform` | 16 | 0 | 0/0/0/0 |
| `loop-xform-after-{1,3,7,64}` | 60 | 50 | **50**/0/0/0 |
| `loop-xform-after-1000` | 51 | 40 | **40**/0/0/0 |

`default` is the healthy shape the counters were built to make legible: entries
taken and **no** exits. The forced-exit arms are the livelock shape on purpose.

### The injection

A green differential is worth exactly as much as its ability to go red. The
recorded defect (`jit-osr-bail-reruns-loop-iterations`) was reintroduced by hand
— `transfer_osr_exit_into_live_frame_checked` was patched to skip writing the
JIT-advanced locals back, so the interpreter resumes from its own stale
pre-entry values — and the harness reported:

```
exit-test       DIVERGED
  < shape=nestedLoops   n=200000 execs=201096 …
  > shape=nestedLoops   n=200000 execs=203661 …
exit-after-7    DIVERGED
  > shape=intSum        n=200000 execs=200006 result=19999900000
```

**200 000 requested, 200 006 executed** — the same signature as the historical
"20 000 requested, 20 008 executed". And `intSum`'s `result` was
**byte-identical** in both arms, because the sum is a pure function of `n`:
precisely the weak check the lane's history records as having missed the defect.
The patch was reverted and never committed.

### Vacuity, refused structurally

The harness passes `CRATONVM_DBG_JIT_METHOD_STATS=1` to every CratonVM arm
(uniformly, so the arms stay comparable; it writes to stderr, which the
comparison does not read), parses the OSR lifecycle line, and **fails the run**
when a forced-exit arm took no entry, or took entries and no exits, or when
either of the two cross-check rows is non-zero. An arm that produced no
accumulator line is scored `NO-OUTPUT`, never as a match.

---

## 3. `osr_exit_points`, cross-checked — and what the cross-check found

The brief: *"`osr_exit_points` exists and is populated under the bytecode
transform, but nothing cross-checks it against where exits are actually taken."*

Writing the check found that **`osr_exit_points` is not what its name says**.
It is written by `Compiler::emit_osr_exit_map_at_reason`, which is shared by two
emitters — the loop-boundary map (`DeoptReason::OsrExit`) and the
`invokedynamic` uncommon trap (`UnreachedCode`). So membership alone cannot tell
*"a whole number of iterations completed"* from *"the body hit a site it can
never execute"*, and those are exactly the two readings the lane wants apart.
The classification therefore consults the recorded **reason** as well as the
set, and the four rows partition the exits that arrive with a reconstructed
frame:

| row | meaning | expected |
|---|---|---|
| `osr_exit_at_loop_boundary` | in the set **and** reason `OsrExit` | the healthy exit |
| `osr_exit_off_loop_boundary` | a real point that is not a loop boundary (an indy trap, a BCE guard) | legitimate |
| `osr_exit_map_missing` | an `OsrExit` point **not** in the set | **zero** |
| `osr_exit_bci_unrecorded` | neither set names the bci | **zero** |

The last two are cross-checks between metadata one function writes, not
classifications. Both read zero on every arm above, including every
loop-transform arm — which is the interesting case, because the transform moves
both vectors between coordinate spaces and `driver.rs` maps only one of them
explicitly (`LoopXform::bci_at`).

A second, smaller correction landed with it: **`deopt_real_enabled()` is
default-ON** (`Err(_) => true`), so `osr_exit_points`, `can_osr_exit` and the
in-place transfer are live production paths. Several comments in the tree still
say these are "empty unless `CRATONVM_DEOPT_REAL`"; they describe an opt-in that
became an opt-out. Nothing was changed for it — it is recorded here because the
cross-check would read as inert diagnostics for a disabled feature otherwise,
and it is not.

---

## 4. "What to refuse" — at admission, and narrowed by a measurement

> An OSR exit whose resume bci has more than one possible native image, or none.

**Where.** At *admission* (`osr_exit_policy`), not at exit. Refusing at exit is
not the mirror of refusing at entry: by then the body has committed iterations
and the caller's only remaining move is the safe reject, which re-runs all of
them — the exact defect this lane exists for. So a refusal added at exit would
be a way to *cause* the defect. `resume_after_exit` keeps an ambiguity arm, as
an assertion of something admission already guaranteed, on the same
"unreachable rather than merely rare" footing as its `MaterializationRequired`
guard. Tag: `osr-entry-ambiguous-exit-image`, permanent (a pure function of the
artifact) and therefore memoable.

**What.** The first draft's agreement predicate was `(semantics, reason)`.
Running it decided the question the other way:

> `CratonBench`, 2026-08-04: **10 of 11** OSR entry refusals were
> `osr-entry-ambiguous-exit-image` on `CratonBench.matrixKernel(I)I` at
> `bci 16`, where the loop-boundary map (`+0x37e`, `OsrExit`) and the
> speculative-BCE range guard (`+0x3c1`, `BoundsCheck`) share a header bci.
> They **agree on `ResumeSemantics`**, and the refusal is memoed, so it cost
> that kernel its OSR for the life of the process.

A loop header carrying both its exit map and its range guard is the ordinary
shape of a compiled counted loop. So the predicate is now **`semantics` alone**:

* a `semantics` disagreement **refuses** — one image saying the bytecode at the
  bci has not taken effect and the other saying it has makes the resume bci
  itself arbitrary. That is the wrong-code half.
* a `reason`-only disagreement is **counted, not refused**
  (`osr_entry_reason_ambiguous_image`, and it is *not* expected to be zero).
  Its only consumer is the de-speculation lookup at the OSR-exit **reject**
  sink, and an admitted entry cannot reach that sink.

After the narrowing, the same workload: `osr_refused_entry` **11 → 0**,
`osr_entered` **507 → 509**, `osr_entry_refused_ambiguous_image` **10 → 0**.

**Which `semantics` disagreement is reachable** is not the obvious one, and it
is written into both the module and the test. A `RESUME` point anywhere in an
artifact is already refused wholesale by `osr_exit_policy`'s per-point rule (its
successor bci is not computable in the `jit` crate), so it never reaches this
check. A `RETHROW` point is explicitly *allowed* to exist — such points are
stashed via `take_exceptional_frame` and never routed to a resume. So the
reachable case is `REEXECUTE` vs `RETHROW`: a `PendingException` point sharing a
bci with a loop-boundary map. The first version of the test used `RESUME` and
failed with `osr-entry-unresumable-exit`, which is how this was found.

Two over-refusal directions have tests of their own: agreeing copies (what a
loop transform produces — every consumer that *reconstructs* a frame finds its
point by native offset or through the copy's own baked box) and the measured
reason-only pair.

---

## 5. The counters are readable now

The design doc's own scope note: *"There is no CLI surface that dumps
`MetricsSummary` at exit yet, so the counters have not been read end to end from
a live run."* `dump_method_stats_to_stderr` (`CRATONVM_DBG_JIT_METHOD_STATS=1`)
now prints all ten rows, before the `DIAG_CORE` early return so a run with no
tiered manager still reports them. The harness depends on it, which is what
keeps the print from rotting.

Read `osr_exited` against `osr_entered`, never alone. The two being close
together *is* the livelock; `osr_entered` at zero under a hot loop means
requests are refused or declined, and the other rows say which.

> An identical line is added by the unmerged `fix/c2-osr01-entry-metadata-*`
> branch, alongside an admission-gate line that is that lane's own. Whichever
> merges second resolves one `eprintln!` block; the two are the same change.

---

## 6. What is NOT closed

* ~~**The differential is a Java-level oracle, not a frame comparator.** It
  observes the resumed frame through the program's behaviour — per-execution
  side effects and a per-iteration digest of the loop-carried state — not by
  reading the interpreter's slots and diffing them against a recorded
  uncompiled run. That is a real distinction: a divergence in a local that the
  remainder of the loop never reads would not be seen. It is also why the
  injection test matters, and the injection *was* of exactly that kind (drop
  every local write) and was caught, because a loop's live locals are by
  definition read by the loop.~~ **CLOSED 2026-08-04**, §8.
* **`osr_exit_off_loop_boundary` has not been observed non-zero.** Every exit in
  every arm here landed on a true loop boundary. The row exists because the indy
  trap can produce one; nothing in this lane drove one.
* **The de-speculation reason lookup is still by bci and still takes the first.**
  `osr_entry_reason_ambiguous_image` measures how often that pick is arbitrary
  (non-zero on CratonBench). It is tolerated because admission makes the sink
  that reads it unreachable for an admitted entry — an argument about
  reachability, not about the lookup being right. If a future change makes that
  sink reachable, this becomes a defect and the counter is where it will show.
* **`osr-02`'s neighbours.** `osr-01`'s remaining item — the second compile
  door, `compile_osr_artifact` calling `x64::compile` directly — is untouched
  here and is that lane's.

---

## 7. Files

| | |
|---|---|
| `jit/src/osr_exit.rs` | new — the exit-site classification, the resume-image predicate, and the argument for both |
| `jit/src/lib.rs` | the admission refusal, the exit-side assertion, `classify_osr_exit_site` |
| `jit/src/metrics.rs` | six new `OSR_EVENTS` rows |
| `jit/src/tiered.rs` | print the OSR lifecycle |
| `vm/src/runtime/interpreter/jit_bridge.rs` | classify + count every exit that arrives with a frame |
| `probes/OsrExitDifferentialProbe.java` | new |
| `regression-suite/perf/osr-exit-differential.sh` | new |
| `vm/src/runtime/interpreter/osr_frame_trace.rs` | new — §8 |
| `regression-suite/perf/osr-frame-comparator.py` | new — §8 |
| `regression-suite/perf/osr-frame-differential.sh` | new — §8 |

---

## 8. The frame comparator — the item taken literally

Branch `fix/c2-osr02-frame-comparator-20260804`. §6's first residual, closed the
same day it was written, because "delivered in substance" is not the same claim
as the brief's:

> compares the resumed frame against the frame an un-compiled run would have had
> at the same iteration count

§2's oracle is behavioural. This one reads the frames.

### The trace

`CRATONVM_DBG_OSR_FRAME_TRACE=<class-substring>` emits three record kinds in one
format, so a checker compares them without knowing which produced which:

| | |
|---|---|
| `A` | a back-edge **arrival**, with its per-`(method, bci)` index |
| `E` | the frame an OSR **entry** was taken with |
| `X` | the frame an OSR **exit** transferred into the live frame |

Locals go through the same `get_local_raw` / `get_local_tag` pair the OSR entry
contract reads, so what is compared is what OSR itself acts on rather than a
re-derived view. The stack comes from `ValueStack::snapshot_raw`. Both render as
`tag:word`, hex, in slot order.

Two placements are load-bearing:

* **The arrival hook is in `try_osr_with_backoff`, ahead of every early return.**
  That function is the one funnel all fourteen back-edge sites go through, and
  the ground-truth arm runs `--nojit`, where the virtual-thread test, the
  `CRATONVM_JIT_OSR` gate and the backoff schedule each decline. A hook after any
  of them would emit in one arm and not the other, and the comparison would be
  between two different things rather than two runs of one.
* **The exit hook is after the write, and reads the frame, not `rframe`.** They
  differ: an `Unsupported` source slot is deliberately left at the live frame's
  current value, so a record built from the reconstruction would describe a
  frame that never exists.

### The two assertions, and why one was not enough

For each `(key, bci)` site, `osr-frame-comparator.py` maps every record of the
run under test to its index in the un-compiled run's sequence by **exact frame
equality**, and requires:

1. **the index sequence to increase strictly** — a frame matching nothing is a
   state the program cannot be in (the case §2's oracle misses when the slot is
   never read again); an index that repeats is an iteration executing twice;
2. **`index(X) - index(E) >= --min-advance`** — how far the compiled body got,
   measured on the un-compiled run's own trajectory rather than taken from
   anything the JIT claims.

**Assertion 2 exists because a hand-built fixture walked straight through
assertion 1.** Compiled iterations produce no arrival records, so the historical
defect — entered at frame 5, ran to 12, resumed at 5 — and "entered at 5 and
advanced nothing" are the *same* index sequence: 0…4, then 5, then 6, 7, 8…
Strictly increasing, and wrong. Only the `E` record separates them. Zero advance
is *correct* for the unconditional-at-header trigger, where reject and transfer
coincide, so the floor is a parameter rather than a constant.

A **gap** between an exit and the next arrival is not an error at all. That gap
is the OSR; its size is reported, not judged.

### The checker is itself guarded

`--selftest` builds six transcript pairs with known verdicts — `clean`,
`replay`, `corrupt-local`, `backwards`, `no-exit`, `no-truth` — and the
differential script runs it **before** the real comparison, so a comparator that
has stopped catching anything cannot report a green run. Two of the six are
vacuity cases rather than defects: an empty ground truth (the class filter
matched nothing) and a run under test with no exits (no OSR happened, so nothing
about OSR was tested). Both must be red.

### What it still does not cover

* **Only frames at a loop header.** The trace records arrivals at back edges and
  the resume point, which is where OSR entry and exit happen. A divergence
  introduced and repaired *within* one iteration is invisible to both halves of
  this differential.
* **The index is ambiguous on a site whose frames repeat.** A loop whose header
  state is not monotone has several candidate indices; the comparator takes the
  smallest one that makes progress and **marks the site `ok*`**, because
  "strictly increasing" is a weaker claim there. Reported, not hidden.
* **Volume.** One line per back edge means the ground-truth arm is large; the
  driver lowers the trip count and `CRATONVM_TIER_OSR_BACKEDGE` together so a
  few thousand trips still produce many entries and exits, and per-site records
  are capped with the truncation announced.
