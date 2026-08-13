# The young walk treats a run of EMPTY objects as corruption at other sites

**Status:** the parallel-sweep half is **RESOLVED** (2026-08-13):
`par_accepts == par_attempts` on every cycle of every run, and all seven
chunk-bail reasons and all three zero-run refusal reasons read zero. **Five
latent sites remain** — walks that still misread the shape but that a census
measured at exactly zero on the only repro available. Those are what keeps this
page open.

## Background

An empty object — `ClassId(0)`, `kind = Object` (tag 0), `num_slots = 0` — is
HEADER_SIZE all-zero bytes, because `MARK_NEUTRAL`, `ObjectKind::Object` and
`ArrayElementType::Reference` are all `0`. Eight `zero_run_end` callers in
`gc/src/gen_heap.rs` shared the rule "an all-zero run of at least HEADER_SIZE at
a walk-grid offset is evidence the grid broke", and for that shape the rule is a
false positive.

Three waves closed it, each one narrowing the question with a census rather than
a guess:

1. the sequential sweep's copy — the hang, `young-sweep-empty-object-run-unwind-20260812-FIXED`;
2. `sweep_chunk` and `clear_all_mark_bits_in_arena` — the two of seven siblings a
   census showed firing;
3. the ragged tail, below.

## Wave 3: which check, and then which condition

After wave 2 the parallel sweep was accepted on ~74% of attempts. `par_accepts`
is a per-cycle verdict over seven different checks, and one `None` from any
chunk discards the whole attempt, so the shortfall named nothing. Splitting it
(`PAR_CHUNK_BAILS`) over eight runs:

```
overshoot=0 gap_filler=0 zero_span=26..85 bad_size=0 hole_crossing=0
phantom=0 anchor_miss=0
```

**Every** bail was the zero run. Splitting *that* (`ZERO_RUN_REFUSALS`):

```
misaligned=4..8 live_inside=0 implausible_next=0
```

**Every** refusal was the alignment test — the predicate's cheapest condition,
and the one carrying the least evidence.

### The trap in that second census

`live_inside=0` did **not** mean no live base was inside the run. The alignment
test ran first and returned immediately, so the informative check never got to
ask. A cheap test ordered in front of an expensive one makes the expensive one's
zero unreadable, and that zero is exactly what a reader will quote. The
predicate now truncates first, so both remaining conditions judge the same
boundary and their counts mean what they say.

### What a ragged run actually is

A run can only be ragged by 8 bytes — the walk is word-granular. That happens
when the object *after* the empty ones also has a zero first word, i.e. it is
itself a `ClassId(0)` empty object whose **mark word** is not zero.
`zero_run_end` stops 8 bytes inside that header, so `run_end` is not an object
start — but `cursor + n * HEADER_SIZE` is, and it is precisely that object's
base.

A one-off probe classified every ragged tail over six runs as neither
side-marked, nor header-marked, nor aged: in practice it is a minted identity
hash or a lock word, i.e. a **dead** empty object that was used before it died.
So truncating does not merely unblock the walk, it resumes on garbage the sweep
then reclaims.

The first attempt at this test asserted the wrong mechanism — that the tail was
an *interpreter*-allocated object, whose mark word was assumed non-zero. The
fixture assertion (`the_interpreter_empty_object_has_a_zero_first_word_and_a_live_second`)
failed immediately: the bare constructor yields a wholly zero header, so the run
it was meant to make ragged was not ragged at all. That assertion is kept.

### Result

Truncating the run down to the last whole slot, and judging *that* boundary
against the live-base and plausible-next-header checks:

| six runs, same class, `-XX:+UseGenerationalGC` | before wave 3 | after |
|---|---|---|
| `par_accepts` / `par_attempts` | 17 / 23 | **4/4, 4/4, 2/2, 3/3, 2/2, 3/3** |
| chunk bails, all seven reasons | `zero_span` 26–85 | all **0** |
| zero-run refusals, all three | `misaligned` 4–8 | all **0** |
| `zero_spans` (sequential unwind path) | 2–63 | **0** |
| `phantom_extents` / `live_in_dead` | 0 / 0 | 0 / 0 |

`zero_spans=0` is the stronger statement: the sequential walk's unwind path —
the thing that caused the original hang — no longer fires at all on this
workload.

## Still open

**Five latent sites.** The selective-promotion evacuation pre-pass, the second
pre-pass walk, `mark_young_to_old_refs`, `fixup_young_old_refs` and
`walk_young_objects` still carry the original rule. A census measured each at
exactly **0** on this repro, so wiring them to `zero_run_empty_object_resume`
would be an unmeasurable change at real risk — but the shape is still misread
there, and a workload that reaches them would pay for it. The predicate they
need already exists and returns a resume point; what each one lacks is the live
set to pass it (`walk_young_objects` has none at all).

**Skipped empty objects are never reclaimed.** A run that the predicate accepts
is stepped over, not parsed — deliberately, since the span may be a live
allocation whose header a stale register-held reference clobbered. That leaves
16 bytes per dead empty object retained until a moving cycle resets from-space.
Bounded and self-limiting (the same objects are re-skipped each cycle rather than
accumulating), and untouched by these three waves. Parsing them instead would
reclaim it, and is a separate safety argument from the one made here.
