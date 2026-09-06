# G1's parallel evacuator retired its forwarding tags one phase before the serial drivers do

| | |
|---|---|
| **Status** | FIXED 2026-09-06. Reproducible on the fixed binary with `CRATONVM_G1_RETIRE_FORWARDS_LATE=0`, which is how the table below was measured. |
| **Scope** | `--XX:UseGc G1`, both PARALLEL evacuation drivers — the default arm. The three serial drivers never had it. |
| **Was** | `known-issues/springboot/g1-parallel-evacuator-corrupts-live-references-20260906.md` (OPEN, 5 of 6 runs) |
| **Reproducer** | `IntegrationAutoConfigurationTests`, one Spring Boot class, ~5 minutes, no crash |

## The defect in one sentence

Both G1 evacuators answer *"where did this object go"* with the forwarding tag
in the from-space object's mark word, and the parallel drivers cleared those
tags one phase before the serial drivers do — with `resurrect_dead_finalizers`,
which still evacuates, sitting in the gap.

## Why a retirement point is load-bearing

Since F-02 the tag is the ONLY test. `evacuate_object` says so:

> the "already forwarded?" test is a LOAD AND A TAG COMPARE, not a hash probe

`pointer_map` is the pause's OUTPUT — what the VM's root, monitor, JNI-handle,
dedup-table and mark-worklist remaps consume — not the collector's own lookup
structure. And the tags have to be retired before the pause ends, or the next
cycle reads one as a this-cycle answer. `retire_forwards` says where:

> Must run AFTER Phase 4 (which resolves forwards) and BEFORE Phase 5.

The three serial drivers do that. The two parallel drivers retired at the end of
`parallel_evacuate` instead — the tail of Phase 3.

## What sits in the gap

Phase 3.5, `resurrect_dead_finalizers`. It was serial-only until F-01, which
removed the `pending_finalizer_roots.lock().is_empty()` term from the young
dispatch on the argument that

> Phase 3.5 now runs on the parallel drivers too, after `parallel_evacuate`
> returns and while the regions guard is held again, **which is the same state
> the serial path runs it in**.

That clause is the defect. The serial path runs Phase 3.5 with the forwarding
tags still installed; the parallel path ran it with them already retired. So
every object the closure had already copied looked untouched to Phase 3.5's
transitive scan: it copied each one AGAIN, and `pointer_map.insert` overwrote
the correct entry with the duplicate's address.

Nothing crashes. `retire_forwards` deliberately preserves the shape quartet, so
the abandoned body stays walkable and the duplicate is a faithful copy. What
breaks is IDENTITY — one object, two live copies, some holders rewritten to
each. Before F-01 the two could not meet: a pause with finalizer candidates was
forced onto the serial arm.

## Measurement — one binary, one variable per arm

`IntegrationAutoConfigurationTests` on `azureuser@20.80.105.49`, dev
`785e777cb`, one process at a time, round-robin so load drift lands on every arm
equally. `reevac` is the new `REEVACUATED_AFTER_RETIRE` warning line count; the
warning fires on the first eight events and then on powers of two, so 17 lines
means the counter passed 4096.

| arm | `RETIRE_FORWARDS_LATE` | `REEVAC_GUARD` | rep 1 | rep 2 | rep 3 | reevac lines |
|---|---|---|---|---|---|---|
| `g1-dev` (= shipped dev) | 0 | 0 | **FAIL** | **FAIL** | **FAIL** | 17 / 17 / 17 |
| `g1-late` | 1 | 0 | PASS | PASS | PASS | 0 / 0 / 0 |
| `g1-guard` | 0 | 1 | PASS | PASS | PASS | 16 / 13 / 16 |
| `g1-fix` (default) | 1 | 1 | PASS | PASS | PASS | 0 / 0 / 0 |

Every failure is `SBRUNNER_RESULT tests=34 failed=1`, the page's signature.
Wall time is 256-341 s on every arm, so nothing here is a timing effect.

What each row settles:

* **The counter IS the defect.** It is non-zero on exactly the arms that keep
  the old retirement point, and the test fails on exactly the arm where nothing
  answers the re-evacuation.
* **Moving the retirement removes it** (`g1-late`: 0 events, 3/3 pass).
* **Answering from `pointer_map` also removes the symptom** (`g1-guard`: the
  events still happen, 13-16 lines, and the test passes anyway). That is what
  makes the guard a net rather than a duplicate of the fix — it converts the
  same population without depending on the ordering being right.

### What one pause actually did

```text
[g1] Phase 3.5 resurrection RAN (#1): resurrected=2 copied=18432
[g1] evacuation asked to RE-EVACUATE 0x7244c5319e80 (#1): this pause already
     forwarded it to 0x7245236a33e0, but its mark word no longer carries the tag
```

TWO dead finalizables, and the subtree walk behind them re-copied eighteen
thousand objects — over four thousand of which the counter caught as objects the
closure had already moved.

The failure that run produced was

```text
AnnotatedConnectException: finishConnect: Connection refused: localhost/127.0.0.1:0
```

a port field that reads back **0**. The page recorded a different reflection
failure every run; a port of zero is the same shape seen from a different
holder — whoever configured the object and whoever read it were holding
different copies of it.

## The page's other three candidates

Its "Next" section named four differences between the drivers. Three are not
differences:

* **TLAB-carved to-space placement and `retire_tlab`'s cursor write-back.**
  Every worker retires both its TLABs at the end of `run_worker`, the driver's
  own included (it participates as a worker), and the serial drain TLAB is
  retired by `retire_all` before the pointer map is built. The write-back is
  complete on both arms. Placement does differ — the parallel arm carves only
  from the Free pool — but that is region consumption, not correctness.
* **The fused seed/closure ordering.** It is not fused. The driver runs Phases
  1, 1b and 2 alone; the helpers only start at `pool.scope`. Seeding order is
  identical to the serial arm's; only the drain is parallel.
* **Where `resurrect_dead_finalizers` runs.** Same protocol point on all four
  drivers since F-01. What differed was the STATE it ran in, which is the
  candidate below.

The fourth — **forward retirement inside `parallel_evacuate` rather than between
Phases 4 and 5** — is the defect.

## A second hole the same move closes

Phase 3.5 installs forwards of its own, through the serial `evacuate_object`.
With retirement at the end of Phase 3, nothing retired them: they outlived the
pause, in a KEPT region by construction, where the next cycle's fast path reads
a stale forward as a this-cycle answer. Retiring after Phase 4 uses the
`pointer_map` Phase 3.5 has already extended, so those are covered too.

This is the question the flat-walk corrupt-cell note in `g1.rs` asks and could
not answer — *"who leaves a forwarding pointer where a walk later reads a
`class_id`, and why does `retire_forwards` not reach it?"*

## The fix

* `gc/src/g1.rs` — the parallel young and mixed drivers call `retire_forwards`
  between Phase 4 and Phase 5, exactly where the serial drivers do. The private
  copy of the retirement loop inside `parallel_evacuate` is gone; there is one
  `retire_forwards` and one argument for preserving the quartet.
* `evacuate_object` gained a fail-closed backstop: when the mark word carries no
  tag but `pointer_map` already names the address, it answers from the map
  instead of copying, and counts it. One hash probe on the path that is about to
  memcpy an object — not the per-slot cost F-02 removed.
* The CAS-loser arm on the EVACUATION-FAILURE path now records its adopted
  forward, as the two sibling arms already did. It was the third arm of a shape
  that had been fixed twice, and the one where a missed record is worst: a
  self-forwarded object's region is KEPT, so its tag outlives the pause.

### Flags

| flag | default | what `0` does |
|---|---|---|
| `CRATONVM_G1_RETIRE_FORWARDS_LATE` | on | restores the old retirement point — reproduces the defect |
| `CRATONVM_G1_REEVAC_GUARD` | on | drops the `pointer_map` backstop; the counter still counts |

### Tests

* `phase_3_5_does_not_split_the_identity_of_an_object_the_closure_copied` — one
  live object named by both a root and a dead finalizable; the root and the
  resurrected finalizable must reach the same address. It asserts IDENTITY, not
  the counter, so it holds however the check is implemented.
* `the_parallel_driver_also_leaves_no_forwarding_tag_behind` — the parallel twin
  of the serial retirement test, which drove `young_collection_serial`
  explicitly *because* the parallel arm retired somewhere else. A mirror that
  moves in only one direction is how the serial test once passed with
  `retire_forwards` deleted.
