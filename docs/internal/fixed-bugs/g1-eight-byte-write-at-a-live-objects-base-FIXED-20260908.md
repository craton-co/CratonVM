# G1: the eight-byte write at a live object's base is a FORWARDING INSTALL at an address that is not an object start

| | |
|---|---|
| **Status** | **FIXED, 2026-09-08.** Two independent doors let an address that is not an object start reach `evacuate`, which sizes an object from the bytes it finds there, copies them, and installs a forwarding mark word at `addr + 8`. That third write is the "eight bytes at a live object's base" this page is named for; the copy is the arena-pointer holder family beside it. Both doors are closed, both are ablatable, and the closure is measured: **corrupt cells in 6 of 19 ablated runs and 0 of 19 default runs** on the config that reproduces, with the screen engaging in 13 of those 19. |
| **Symptom** | `OutOfMemoryError` / `ClassCastException` (JIT and interpreter shapes) / `EXCEPTION_ACCESS_VIOLATION` / SIGSEGV on `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentXmlExternalWarXml` and `org.h2.test.store.TestMVStoreTool`, with a corrupt-cell census whose producer six months of read-side instrumentation could not name. |
| **Was** | `docs/known-issues/tomcat/g1-eight-byte-write-at-a-live-objects-base-20260906.md`, whose closing statement was "**the origin is upstream of every walk on this page and is still unfound**". |

## The producer, in one paragraph

`SharedEvac::evacuate` and `G1Collector::evacuate_object` do three things to
the address they are handed: they read a header and size an object from it,
they `copy_nonoverlapping` that many bytes to a fresh destination, and they CAS
a forwarding mark word into `addr + 8`. Handed an address that is **not an
object start**, all three go wrong in ways this page measured for weeks without
connecting:

* the **copy** reproduces a live object's BODY words as the destination's
  header, so the destination's first eight bytes are whatever the victim had at
  that offset — typically a reference payload, i.e. **a pointer into this arena
  sitting where `class_id`/`shape` belong**, with a plausible mark word beside
  it. That is this page's arena-pointer holder family, and it is why those
  holders keep appearing as the FIRST object of a fresh to-space region;
* the **forwarding install** writes eight bytes at `addr + 8`. For an interior
  `addr` that is inside a live object's body, and it leaves the word beside it
  untouched. That is this page's corrupt-cell family, bit for bit:
  `raw0 = target | MARK_FORWARDED`, `raw1 = 0`, at a cell the region's own
  object grid places inside a sound holder.

Every negative result on the old page is consistent with this and none of them
pointed at it, because none of them looked one frame ABOVE the walks. The copy
watch was clean at every checkpoint because nothing rewrote a COPY. All six
flat walks reported the damage because the damage is in the memory they read.
Screening one walk moved the reports to the next because the producer is none
of them.

## Door 1: `forwarding_target` strips two bits, `make_forwarded` asserts three

`ObjectHeader::make_forwarded` asserts `plausible_heap_pointer(target)` —
non-null, **8-byte aligned**, under 2^47 — so every forwarding target any
collector installs has its low THREE bits clear.
`ObjectHeader::forwarding_target` masks off `MARK_STATE_MASK` (the low **two**)
and the quartet. **Bit 2 survives the decode.**

`is_forwarded_mark(w)` is `w & 0b11 == 0b11` and nothing else. So a word that is
not a forwarding pointer at all — Java string bytes, a `long` field, a hashcode
— but whose low two bits happen to be `0b11` passes it, and decodes to an
address no installation could have produced. That value was then returned to
the evacuator as "where this object went", **stored into the reference slot
being scanned**, and **pushed onto the gray worklist as a holder**.

Measured 2026-09-08 by a new tripwire on every reference-slot write
(`TestHostConfigAutomaticDeploymentXmlExternalWarXml`, Linux, `-Xmx1g`,
`CRATONVM_G1_WORKERS=16`, `RESUME_DEST=0`):

```
slot=0x7a24de7ce638 value=0x00006e6547246e6c low3=4 width=8
slot=0x7a24deafd098 value=0x0000636a61636a2c low3=4 width=8 holder_end-holder=0x2d0226278
slot=0x7a24deafd0c8 value=0x0000636a61636a2c low3=4 width=8
slot=0x7a24deafd758 value=0x00000000f5ecdec4 low3=4 width=8
```

**`low3 = 4` on four of four** — the signature of this decode and of nothing
else — and the payloads are ASCII: `0x636a61636a2c` is `"(jacj"`,
`0x6e6547246e6c` is `"ln$Ge"`. Java string characters, read as a mark word,
decoded as an address, written into a live object's reference slot. One of
those holders declares a body extent of **twelve gigabytes**, which is what a
worklist holder made of arbitrary bytes looks like.

`G1Collector::decode_forwarding_target` validates the decode at all four sites
(both evacuators' already-forwarded fast paths and both CAS-loser arms):
alignment plus arena bounds, which is the install-time assertion restated as a
test. Refusing cannot drop a live reference — a word that is not a forward does
not name a relocation — and the callers already handle `None` by leaving the
slot with the address it has.

There is no flag. This is a decode correction, not a policy: a value the
install path asserts is impossible has no arm to be measured against. It fires:
16 refusals in one run of the ablated A arm.

## Door 2: a null `Value` cell's payload word is a valid zero-field object

A legacy `Value` cell is sixteen bytes — a discriminant word, then a payload
word. A NULL reference cell is `[4, 0]`. So the address of its **payload** word
has, as its own next sixteen bytes, `[0, <the next cell's discriminant>]`,
which reads as:

```
class_id = 0   num_slots = 0   kind = Object   element_type = Reference
gc_flags = 0   gc_age = 0      state = NEUTRAL      size = HEADER_SIZE
```

— a valid, sixteen-byte, zero-field object. It satisfies
`classify_candidate_header` (both tag bytes decode; the address is 8-aligned
and below its region's cursor). It satisfies `note_implausible_legacy_header`
(class 0 with a handful of slots is legitimate in this tree, and `0` is not a
pointer into the arena). It satisfies `object_total_size`. **Every header
screen in `gc/src/g1.rs` accepts it**, and the file already said so: the
assertion in `an_ordinary_object_root_does_not_pin_its_region` pinned the
limitation in as many words —

> a zeroed cell decodes as a plausible header — this screen cannot see it, and
> a test claiming otherwise would be describing a collector we do not have

— and that limitation is this defect. The assertion is now inverted rather than
deleted, so the reason survives beside the fix.

`empty_header_is_a_real_object` closes it with the only oracle that can: the
region's own object grid, walked from the base by each object's declared size,
and paid **only** for that one shape. Measured on the first run with it armed:

```
root-pin-scan: REFUSED an EMPTY-HEADER candidate the object grid places INSIDE
another object (#1): addr=0x7d0f3d3ee938 mark=0x0 region=15 type=Survivor
grid=INTERIOR of=0xee898 delta=0xa0 size=0x130 cid=64

(#2): addr=0x7d0f3d288550 grid=INTERIOR of=0x884e8 delta=0x68 size=0x88 cid=1705
```

`delta = 0xa0` is `HEADER_SIZE + 9*SLOT_SIZE + 8`, and `delta = 0x68` is
`HEADER_SIZE + 5*SLOT_SIZE + 8`. **Both are exactly a legacy cell's PAYLOAD
word address**, which is what this door predicts and nothing else does.

All ten refusals in that run came from `root-pin-scan` — the conservative JIT
root scan, which finds a slot pointer in a compiled frame and publishes it as a
root. Before the fix that root passed the pin screen, so its region stayed
collectable; then it passed `note_root_object_plausibility` in the root loop for
the same reason; then `evacuate_object` installed a forwarding mark word at
`root + 8`. The chain is complete and every link is measured.

`CRATONVM_G1_EVAC_EMPTY_HEADER_GRID_PROOF=0` restores the old behaviour, and
that is the A arm below.

## The A/B

One binary, arms interleaved per repetition, `-Xmx1g`,
`CRATONVM_G1_WORKERS=16`, `CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST=0` — the
config the old page identifies as reproducing N/N, with worker count as the
lever rather than heap size. 19 pairs across three binaries and a host load
that ranged from 3 to 60:

| arm | runs | runs with corrupt cells | corrupt cells | FAIL / CRASH |
|---|---:|---:|---:|---:|
| **A** — `EVAC_EMPTY_HEADER_GRID_PROOF=0` | 19 | **6** | **70** | 1 FAIL |
| **B** — default | 19 | **0** | **0** | 0 |

Fisher exact on 6/19 against 0/19 is p ≈ 0.019.

**The B arm is not vacuous**: the screen refused 127 empty-header candidates
across 13 of the 19 default runs. And the arm that produced all the corruption
is the one that ran *without* the screen while keeping every other 2026-09-08
fix, so this table isolates door 2 specifically.

Every A-arm corrupt cell carried `slot_in=HOLDER` — the region's grid places
the corrupt slot inside its own holder — which is the verdict that says a
foreign write landed in a live object's body rather than that a walk
over-strode. Both verdicts exist in the wild and the new field separates them.

On H2 (`org.h2.test.store.TestMVStoreTool`, `-Xmx256m`,
`CRATONVM_G1_JIT_MARK_DRIVER=1`), where the old page records the family
appearing "within a couple of minutes and a few hundred pauses": **0 corrupt
cells, 0 refusals of any kind and no SIGSEGV in 3 × 420 s**. Those runs are
still TIMEOUTs, which is the separate throughput defect the old page already
excludes from its own scope.

## Cost

`empty_header_is_a_real_object` is O(region), so where it is asked from
matters. The two evacuator ENTRY screens ask once per object evacuated —
measured 21 000 to 43 000 empty-header candidates per run against 27 to 46
refusals — while the ROOT and SUPPLY routes ask once per root or seed, and
**every refusal ever measured came from `root-pin-scan`**. The proof is
therefore taken on the cold routes only (`GridProof::Yes` / `GridProof::No`),
with a per-pause budget behind it as a backstop:

| | grid walks per run | waived (budget hit) | refusals | wall clock |
|---|---:|---:|---:|---|
| proof on every route | 2304 – 3723 | **19 039 – 39 171** | 27 – 46 | A 178.6 s / B 180.8 s |
| proof on the cold routes | **48 – 208** | **0** | 0 – 8 | A 132 s / B 129 s |

Confining it drops the walks fifteenfold, takes the waivers to zero — so the
screen now covers every candidate it is asked about rather than the first 64 of
a pause — and costs nothing measurable.

## The three screens that were missing, and one that only warned

1. **Three evacuation SUPPLY routes had no screen of their own** — the marking
   keep-alive set (`marking_keepalive_roots` filters on CSet residency only),
   the evacuation-failure drain's seeds (`drain_kept_self_forwards`, whose seeds
   are the identity keys of a FAILED pause's forwarding map) and Phase 3.5's
   finalizer resurrection. The root loops have screened since 2026-09-02 and the
   two worker walks since 2026-09-05; these three were left, and
   `SharedEvac::evacuate`'s last-ditch guard checks only ALIGNMENT, which an
   interior address passes trivially. An object-start screen now sits at BOTH
   evacuator entry points, where it covers every route at once, and
   `#[track_caller]` makes the report name the route that offered the address.
   It fires: `grid=INTERIOR of=0x2fcd0 delta=0x18 size=0x20 cid=1796`, and
   `grid=INTERIOR of=0x170a0 delta=0x4e1e0 size=0x7a250` — 26 in one run.
   (`CRATONVM_G1_EVAC_SUPPLY_SCREEN`, default ON.)

2. **The arena-pointer CANDIDATE screen ran and threw the answer away.** The old
   page recorded this for `evacuation_candidate_is_an_object` and left
   `CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE` opt-in, on the reasoning that "a
   refusal that is WRONG drops a live reference". That reasoning was taken
   against the class-id BAND test — an assumption about which ids a loader
   mints, which had already gone stale once — and the ARENA test was folded into
   the same flag afterwards. They are not the same kind of claim:
   `class_id`(4) + `shape`(4) IS the header's first eight bytes, so a pair
   landing inside this collector's own arena is a BODY word read at an address
   that is not an object start, and no future id space can make it a header. The
   same test already refuses by default on the serial evacuator's holders and on
   the Phase-4 fixup's. It is now split out and default-ON
   (`CRATONVM_G1_EVAC_CANDIDATE_ARENA_SCREEN`); the band refusal stays opt-in,
   unchanged. Measured firing, with the predicted shape:

   ```
   REFUSED a candidate whose first header word is an ARENA POINTER (#1):
   holder=0x711f3b74cf90 slot=16 candidate=0x711f3afee940
   paired=0x0000711f3b300003 mark=0xc021000000000000
   ```

   `paired` is itself `<arena address> | MARK_FORWARDED` — a mark word read
   where a header belongs, at `realobject + 8`. 17 in one run.

3. **`update_object_refs` had no bound on either arm, and it WRITES.** The old
   page names it the AMPLIFIER: one refusal in its census named a holder
   declaring 102736 legacy slots, a 1.6 MB body in a 1 MiB region, rewritten
   cell by cell with forwarding addresses. Its array arm iterated `array_length`
   raw `u64` slots and its object arm went through
   `for_each_flat_object_reference_trusting_header`, whose own doc says it
   "bounds it by nothing". Its single caller happens to break on
   `offset + obj_size > cursor`, so in practice the counts already fit — but
   that is a property of ONE caller, restated nowhere, and the counts come from
   the same `shape` dword this family corrupts. Both arms are now clamped to the
   holder's own region, which cannot drop a live reference because a reference
   field of an object is inside that object.

## Three instrument defects the old census rested on

These are corrected. The old page's numbers should be re-read with them in mind
rather than quoted.

### `word0_plausible_ptr` was the wrong test, and it is the field the 19-of-19 rests on

It was `cratonvm_types::plausible_heap_pointer(word0)`, which asks only
non-null / 8-aligned / under 2^47. It answers TRUE for `0x0000001200000040` —
an ordinary `class_id = 64 / num_slots = 18` header pair, which against the
arena bounds the VM now prints on the same line sits **2157 GiB below
`arena_base`**. The old page re-took that statistic itself and got 0 of 25. The
report now carries the arena test the refusing screens use, with the loose one
renamed `word0_plausible_ptr_LOOSE` beside it, so the two can never again be
quoted as one number.

### The corrupt-cell attribution was a PROCESS-GLOBAL counter under parallel workers

The report was keyed on `cratonvm_types::cell_census::decoded()` moving across
one cell read, with the (correct) reasoning that a corrupt cell decodes to
`Value::Object(None)` and so cannot be told from a genuine null by its value
alone. The mechanism was wrong: `cell_census` is process-global and this walk
runs on **every evacuation worker at once**, so under `CRATONVM_G1_WORKERS=16`
one worker's corrupt cell moved the counter that a different worker then
attributed to whatever cell it had just read.

Measured 2026-09-08: of 20 grid verdicts in one run, two of the first three
named cells holding `raw0=0x4 raw1=<a heap pointer>` — an ordinary, VALID
`Value::Object`, one of which the reference-write watch showed the same pause
had written itself. **The population this family has been counted from is
mixed, and the mixing rate rises with worker count.** It is now keyed on the
cell's own discriminant (`w0 as u32 > VALUE_MAX_DISCRIMINANT`), which is
`read_value_checked_atomic`'s own test applied to the sixteen bytes already
read: exact, thread-local, free.

### The grid drain did not reach this class's pause path

The old page's sharpest open question — "are the corrupt-cell reports evidence
of corruption at all?" — went unanswered on the workload its evidence rests on,
and it says so: **0 verdicts against 14 corrupt cells** at `-Xmx2g`, because the
drain was wired into five pause bodies and this class's reports arrive on a path
that reaches none of them. It now also runs at `collect_garbage`'s single
funnel, which every young, mixed and retry path returns through. The holder
REFUSALS — the population the old page calls "the genuine shape" — are queued
for it too, so they get a verdict for the first time.

## The instruments that found it

* **`CRATONVM_G1_REF_WRITE_WATCH=1`** — a per-pause ledger of every
  reference-slot write the collector makes, keyed by slot address, so "who wrote
  this slot" is a lookup rather than an inference. `CopyWatch` could not answer
  it: it records COPIES, and a write into a resident object's body is not one,
  which is why it read clean at every checkpoint while holders kept appearing.
* **An always-on tripwire** on any reference-slot write whose value is not
  8-aligned. Four hits, `low3 = 4` on all four, ASCII payloads — that is what
  named door 1. It costs a mask and a branch on a path that is already storing
  to memory.
* **`grid_object_containing`** — the per-address companion to
  `grid_closes_on_cursor`. The closure test is a property of the whole region,
  and a holder that over-declares while a later object under-declares still
  closes; this says whether the CORRUPT SLOT is inside its own holder or inside
  the object after it. `slot_in=HOLDER` sends the search to a writer;
  `slot_in=NEIGHBOUR` says the walk over-strode and the report is a misparse.
  Both populations exist and the field separates them.
* **`target_mark_is_raw0` / `slot_minus_8`** on the drain line, which is what
  ruled out "something copied a mark word" and left the forwarding install.

Every counter is printed unconditionally in
`gc_metrics::collector_decision_report` as `[GC] g1 evac-screens:` (under
`CRATONVM_GC_STATS=1`), so a zero is citable evidence rather than a silence —
the failure mode
`the-g1-guard-counters-were-printed-only-on-the-arm-junit-never-takes-FIXED-20260906`
records for the previous ten.

## What this does NOT close

* **Throughput.** `TestMVStoreTool` under G1 with the mark driver still does
  thousands of pauses without finishing its CREATE phase, and the three runs
  above are TIMEOUTs. That is
  `docs/internal/performance/h2-mvstoretool-create-phase-is-mutator-side-address-validation-20260907.md`
  — mutator-side address validation, not this.
* **The ZGC `OutOfMemoryError` on MVStore**, which is a refused compactor with
  88% of the heap free —
  `docs/known-issues/h2/zgc-oom-on-mvstore-is-the-unregistered-entry-frame-blocking-compaction-20260907.md`.
* **The conservative JIT root scan itself.** Door 2's addresses arrive from
  `root-pin-scan`, i.e. a compiled frame holding a pointer into an object's
  field cell. The fix makes the collector refuse to treat that as an object
  start and pin the region instead, which is correct and is what the precise
  routes already do. Making the scan not publish interior slot pointers in the
  first place is a JIT-side question and is not this page's.
* **`CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE` stays opt-in.** Only the arena
  half was promoted; the class-id band refusal is still an assumption about an
  id space, and the old page's argument against defaulting it on is unchanged.
* **The same decode hazard exists in the sibling collectors.**
  `gc/src/gen_evac.rs` (lines ~979 and ~1125) and `gc/src/zgc.rs` (~8749) both
  do `is_forwarded_mark` then `forwarding_target` with no validation of the
  result. Neither is on this page's workloads and neither is measured; the fix
  is the same three lines if either ever reports this shape.

## Reproduction

The config that reproduces on Linux, which the old page did not have (it records
the family as Windows-only and notes "on Azure the same class PASSes far more
often"):

```
CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST=0 CRATONVM_G1_WORKERS=16 \
cratonvm --java-home <jdk25> --Xmx 1g -XX:+UseG1GC \
  -c <tomcat test classpath> org.junit.runner.JUnitCore \
  org.apache.catalina.startup.TestHostConfigAutomaticDeploymentXmlExternalWarXml
```

**Worker count is the lever, not heap size** — each worker claims its own region
per pause, and that is what drains the free pool into the evacuation-failure
path. It is also **host-load sensitive**: all 70 corrupt cells in the table
above came from repetitions taken at load 25-60, and 6 pairs taken on the same
binaries at load 3-14 produced none in either arm. Budget six or more
repetitions before reading a zero as anything, and note that a PASS is not
evidence of absence — the corrupt-cell family appears on passing runs too.

`/data/g1ebw-repro.sh` and `/data/g1ebw-ab3.sh` on the Azure host are the driver
and the interleaved A/B used here.
