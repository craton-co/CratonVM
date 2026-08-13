# The JIT arm exhausts the young heap since `HEADER_SIZE 24 -> 16`

## Status
**FIXED 2026-08-07** in `0ea21c07a` (`gc/src/gen_heap.rs`). The record below is
kept as written; this banner answers the question it left open.

### Which of the two readings was right: the span IS a real object

This page listed two readings and said they were "not yet distinguished". It is
the first one, and the deciding evidence is that the discriminator's own
invariant is written down in a comment that the shrink silently falsified.

`gc/src/tlab.rs` still states it: a live `new Object()` is distinguishable from
zeroed arena because **"identity_hash at offset 8"** is its one non-zero header
word. The `HEADER_SIZE` 24 -> 16 shrink made offset 8 the **mark word**, and
`jit/src/x64/objects.rs` deliberately leaves it zero -- *"identity_hash_code
(offset 8) stays 0 (TLAB-zeroed); the lazy-mint contract ... handles it on
demand"* -- then stores two explicit zero dwords there. `MARK_NEUTRAL` is
`0b00` and `ObjectKind::Object` is `0`, so a JIT-allocated `new Object()` is
`class_id 0 / shape 0 / mark_word 0`: an all-zero 16-byte header. The
interpreter still stamps a hash eagerly (H1), which is exactly why `--nojit`
passes and the JIT arm does not.

### Independently confirmed, 2026-08-07 — 10 classes, and no over-freeing

A second session had reached the same three screens from the H2 side and built
a different patch (gate the anomaly on `verified_spans`, the grid the
exact-base oracle already proved). `0ea21c07a` is the better of the two and is
what landed: stepping over the run one header at a time keeps the walk on-grid
**everywhere**, where the grid-proof gate only helped inside a proved span and
left the rest over-retaining. The other patch was dropped; its measurements are
kept here because a GC change that makes the sweep reclaim *more* wants
independent evidence that it does not reclaim something live.

One class at a time, `--Xmx 1g`, real-JDK 25, `659224249` (broken) against dev
tip with this fix:

| class | broken | fixed |
| --- | --- | --- |
| `TestIndex` | **OOM** 100 s | **PASS** 97 s |
| `TestCompatibility` | **OOM** 240 s | **PASS** 206 s |
| `TestOptimizations` | HANG 400 s | **PASS** 201 s |
| `TestTempTables` | **OOM** 84 s | HANG 400 s — no OOM; this class is the throughput factor, not this bug |
| `TestBigDb` | PASS 2 s | PASS 2 s |
| `TestScript` | FAIL + **OOM** 215 s | HANG 400 s, no OOM |
| `TestCases` / `TestOpenClose` / `TestPerfectHash` / `TestCrashAPI` | HANG | HANG (unchanged) |

**Every `OutOfMemoryError` in the set is gone**, and the two classes that still
do not finish now fail the way the retired
`bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807` record predicts —
too slow for the budget, progressing.

Over-freeing was the risk worth grepping for explicitly rather than inferring
from exit codes: `SIGSEGV`, `ClassCastException`, `corrupt Value cell`,
`panicked`, `overlapping` and `double free` appear in **no** arm, and the three
newly-passing classes verify their own query results end to end.

`TestIndex` also isolates the mechanism cleanly, since the three symptoms move
together:

| | broken | fixed |
| --- | --- | --- |
| result | OOM 63 s | PASS 82 s |
| `selective promotion: unwound` | 3 | **0** |
| `moving-young. fallback` | 14 | 10 |

and the `--nojit` arm, which never had the bug, gets faster too (201 s → 132 s)
because the sweep now reclaims instead of abandoning its stretch.

### The damage was the response, not the detection

One 16-byte span sent the walk from offset 7938816 to 241826160 -- **233 MB of
a 256 MB young generation abandoned unswept**. With an empty free list there is
no anchor to recover at, so the sweep reclaimed nothing. Under
`CRATONVM_DBG_GC_OVERHEAD=1` the fingerprint is unambiguous:

```
young_used == young_cap   young_free_list=0   young_largest_free=0
promoted=7529728  freed=7529728        <- freed EXACTLY equals promoted
```

`freed == promoted` every cycle means promotion is young's only exit; old
headroom then bled 209 -> 186 MB until allocation failed with 68 MB live in a
1 GB heap. `clear_all_mark_bits_in_arena` resynced past the stretch too, so
mark bits stayed set behind it.

This also corrects one claim above: promotion does **not** stop entirely. Over
a longer run the same build reports `sp_evacuated=32787898`. Selective
promotion works; it was the *sweep* that reclaimed nothing.

### The fix, and the wrong version of it

Step over the zero run and keep walking, instead of unwinding and re-anchoring.
The span is still never parsed and never freed, so nothing live can be lost.
The conservative half is kept: reclaim decisions taken since the last anchor
are still dropped.

Advance by **whole headers**, never to `run_end`. `run_end` is the first
non-zero WORD, not an object start -- a 24-byte zero run is a 16-byte all-zero
header plus the zero `class_id`/`shape` word of the NEXT object, whose mark
word ended the run. The first revision of this fix jumped there, resumed 8
bytes inside that object, and produced **3486 "implausible object size" aborts
per run** on the 60k arm against **zero** on the unpatched build -- it passed
the small arm and failed the large one. Offsets are 8-aligned, so a run length
is 0 or 8 mod `HEADER_SIZE` and truncating to whole headers lands on an object
start either way.

### Measured

| arm | before | after |
|---|---|---|
| `H2UpdateScaleProbe` 4t x 2500, `--Xmx 1g` | OOM (515 / 222 minor GCs) | **PASS** (4 / 4) |
| `H2UpdateScaleProbe` 4t x 60000, `--Xmx 4g` | OOM 515 s, 5038 minor GCs | **PASS 163 s, 11** |
| `org.h2.test.db.TestIndex`, `--Xmx 1g` | 117650 `OutOfMemoryError` | **0, RC=0** |

ABBA-interleaved. `implausible object size` and `abandoning rest of arena` are
0 in every fixed arm. gc 1023 tests and vm 2454 lib tests green.

## Status (as filed)
**OPEN (2026-08-07).** A/B'd against the merge's own first parent with two
binaries built from this tree, so the attribution is not an inference. Not a
throughput problem: the same class passes with `--nojit`, and passed on the
pre-merge binary *with* the JIT.

## Severity
**HIGH.** `--Xmx` does not help — `TestTempTables` dies at 1 g and at 4 g. Any
JIT-active workload whose live set grows must promote out of the young
generation, and on this tip it cannot.

## The A/B

`org.h2.test.db.TestIndex`, `--Xmx 1g`, real-JDK 25, direct invocation, one
class at a time, Azure host at load 20-24. All four arms ran back to back
inside ten minutes.

| binary | flags | result |
| --- | --- | --- |
| `9ddbc9c61` — the merge's **first parent** | default | **PASS 174 s** |
| `6ba350cdd` — the merge | `CRATONVM_COMPACT_REF_FIELDS=0` | **OOM 79 s** |
| `6ba350cdd` + the two mark-word quartet fixes | `CRATONVM_COMPACT_REF_FIELDS=0` | **OOM 127 s** |
| ... the same binary | default | **OOM 110 s** |
| dev tip `70bf05ed3` + this branch | default | **OOM 63 s** |
| dev tip `70bf05ed3` + this branch | `--nojit` | **PASS 190 s** |
| HotSpot 25 | — | PASS 4 s |

The last two rows are the same tree this record ships on, so the regression is
current, not a snapshot: 190 s with the JIT off matches the first parent's
174 s with it on, and turning the JIT on turns a pass into an OOM.

Three things follow directly:

* **The merge introduced it.** Its first parent passes the same class on the
  same host minutes earlier.
* **It is not this branch's two quartet fixes.** The unfixed merge commit OOMs
  too, and sooner.
* **It is not the compact reference-field layout.** It OOMs with
  `CRATONVM_COMPACT_REF_FIELDS=0`, so the cause is the header shrink and the
  quartet's move into the mark word, not the field packing.

`org.h2.test.db.TestTempTables` behaves the same and adds the heap-size and
JIT axes:

| `TestTempTables`, one class at a time | result |
| --- | --- |
| HotSpot 25, `-Xmx1g` | PASS 3-12 s |
| pre-merge-era binary, JIT, `--Xmx 1g` | PASS 694 s |
| this branch, JIT, `--Xmx 1g` | **OOM** 108-227 s |
| this branch, JIT, `--Xmx 4g` | **OOM** 557 s |
| this branch, `--nojit`, `--Xmx 1g` | PASS 633 s |

Quadrupling the heap buys 2.5x the time and still ends in
`java.lang.OutOfMemoryError: Java heap space`, surfacing through MVStore as

```
org/h2/mvstore/MVStoreException: java.lang.OutOfMemoryError: Java heap space
  (lambda proxy with 4 captures) [2.4.249/3]
```

A 16-class sweep of the H2 classes this touches shows the same split: the
`--nojit` arm produces **no** `OutOfMemoryError` at all — every one of its
failures is a plain timeout — while the JIT arm produces three.

## The mechanism, as far as the logs take it

`--nojit` passing is the shape of the thing. With no compiled frames on the
stack the young collection is the ordinary moving (Cheney) one and survivors
are copied and aged normally. With the JIT on, every young collection in these
runs diverts:

```
[moving-young] fallback #N: reason=compiled-frame-oop-not-published
[moving-young] fallback #N: reason=innermost-rbp-belongs-to-unguarded-callee
```

That fallback is pre-existing and separately tracked — it is not the new part.
The new part is that the non-moving sweep's **only** drain stops draining:

```
selective promotion: unwound 7039 candidate(s) collected on a suspect walk
                     stretch (grid anomaly since last anchor)
```

7039 candidates, unwound, and repeated. `unwind_evac` drops every promotion
candidate collected since the last trustworthy anchor, so one anomaly per
stretch means nothing is promoted at all: young never drains into old, and the
heap fills whatever `--Xmx` says.

## The lead: at `HEADER_SIZE == 16` a 16-byte all-zero span is exactly one header

Beside every unwind, the same walk reports:

```
non-moving sweep: unlisted all-zero span at offset 3304 (run 16 bytes,
                  next anchor at 242043560) - skipped, not freed
```

Always **16 bytes**, always at the same low offset. The screen that fires is
`gc/src/gen_heap.rs`:

```rust
let run_end = zero_run_end(from_base, cursor, limit);
if run_end - cursor >= HEADER_SIZE {
    anomaly = true;
}
```

(the selective-promotion evacuation walk; `clear_all_mark_bits_in_arena` and
the sweep walk carry the same test), and `anomaly` goes straight into
`unwind_evac`. `HEADER_SIZE` was 24 before this merge and is 16 after it, so a
16-byte all-zero run went from *"too short to be a header — stride past it"* to
*"an anomaly — unwind the stretch"*. Nothing about the span changed; the
threshold moved under it.

And a 16-byte all-zero run stopped being hypothetical in the same merge, for
the same reason. It moved `kind` / `element_type` / `gc_age` / `gc_flags` into
the mark word, and its own message says the quartet-zeroing store was deleted
because *"zeroing the mark word subsumes it, since a plain object wants kind=0
and element_type=0"*. A legacy (non-compact) object with `class_id == 0` and no
flags now has an **all-zero 16-byte header** — bit-identical to unallocated
zeroed arena. Before the merge, `kind`/`element_type`/`gc_flags` lived in
dedicated header bytes and such an object was distinguishable.

Two readings, wanting different fixes, and they are not yet distinguished:

* **The span is a real object** — the long-running `ClassId(0)` family. Then
  the screen is right to distrust it and wrong to punish the whole stretch, and
  the question is why a live object carries a zero header.
* **The span is genuinely unallocated** and was merely invisible before. Then
  the screen needs a discriminator that does not collapse at
  `HEADER_SIZE == 16`. The message already says the span is *unlisted*, i.e.
  the free list disagrees with the walk — which is the real anomaly, and is
  independent of how long the run is.

`CRATONVM_DBG=cellcorrupt` prints the holder of a corrupt cell (class, kind,
`num_slots`, `gc_flags`, and the raw window) and is the instrument that
separates them; it is what identified the two quartet defects fixed alongside
this record.

## Reproducing

```bash
cd <fresh writable dir>          # H2 writes ./data
CP="<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)"
<cratonvm> --java-home <jdk25> --Xmx 1g -c "$CP" org.h2.test.db.TestIndex
# -> OutOfMemoryError in ~80-130 s. Add --nojit and it does not OOM.
```

Grep the run for `unwound`, `all-zero span` and `moving-young. fallback`; the
three arrive together. Build the first parent for the other arm:

```bash
git worktree add --detach <dir> 9ddbc9c61
cargo build --release -p cratonvm-cli --bin cratonvm
```

## What this is not

* **Not the H2 throughput factor.** That is measured, flat, and retired to
  `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md`.
  This regression sits on top of it and currently masks it: with `--nojit` the
  same classes go back to being merely too slow.
* **Not the two mark-word quartet defects** from the same merge
  (`try_thin_unlock` storing a bare `MARK_NEUTRAL`; `MARK_QUARTET_MASK` two
  bits short of `gc_age`), written up in
  `docs/internal/fixed-suite-bugs/vm/compact-ref-field-layout-corrupts-filechannel-filelock-FIXED-20260807.md`.
  Those are fixed on `dev`, and this OOM both predates them and survives them —
  which is how the third defect became visible at all.
