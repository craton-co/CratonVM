# `TestRandomMapOps` at a small heap — heap corruption with THREE faces, and no reproducer worth bisecting yet

## Status

**OPEN, split out 2026-08-29** from
`bug-h2-testrandommapops-classcastexception-20260821.md`, which retired with
its own titular defect closed: the recorded `ClassCastException` never
reproduced in eleven runs across seven configurations, its seed passes on
CratonVM *and* stock HotSpot 25, and the one live failure that page did
root-cause (the G1 `NoSuchMethodError Object.toArray()`, an unpinned receiver
across a GC point) was fixed on 2026-08-24.

This page carries the row that page could not close: **`org.h2.test.store.TestRandomMapOps`
at `--Xmx 256m` corrupts the heap.**

**And as of 2026-08-29 it is cheap: 9 failures in 9 runs, 22–471 s, on a host at
load ~3** — against the inherited base rate of "roughly one in three runs of
~20 minutes", which was measured on a contended box. See the section below; the
lever is host quietness, not a flag, and it is the difference between a defect
nobody could bisect and one anybody can.

**ONE OF THE THREE FACES IS CLOSED (2026-09-05): the SIGSEGV.** It was
`ZGC-RELOC-DECOMMIT.1` -- the compaction slide memmoving into granules the
give-back had returned -- and it is fixed by `Arena::commit_for_relocation`.
Measured here, interleaved P/F, 10 reps per arm at `--Xmx 256m`:
**SIGSEGV 5/10 pre-fix against 0/10 fixed** (Fisher's exact p = 0.016),
while this page's `rc=1` faces were UNCHANGED at 1 against 2 -- which is the
correct result, since nothing in that fix addresses them. See
`../../internal/fixed-suite-bugs/bug-testlargeblob-segv-decommit-under-live-memcpy-20260904.md` for the root
cause and the guard-ON/OFF A/B that pins it. **Neither arm produced a passing
run**, so this page's standing "no passing CratonVM run at any heap" still
holds and the `NullPointerException` / `AssertionError` faces below remain
OPEN and unexplained. Do not re-chase the segfault.

**READ BOTH 2026-09-02 ADDENDA, THE "(later)" ONE FIRST.** Where things stand:
the fail-closed fix REDUCED this defect and did not close it — the
`NullPointerException` face still reproduces at the shipped default at
`--Xmx 256m` on a quiet host (2 of 3 runs in one batch on a release binary).
`--Xmx 1g` and `--Xmx 4g` are clean over 1500 s each, so the title's "small
heap" premise is right again. And the residue this page treats as evidence of a
missed root has been MEASURED and is not one: `local_oop`, the count that names
a missed root, is zero over ~370 frames. Everything below the 2026-09-01
addendum is the historical record, including three readings this page later
withdrew.

## ADDENDUM 2026-09-01: the COST is gone; the HOLE is not. Both were measured on one binary

Two things this page states as current are no longer true, and one thing it
implies is not true either. All figures below are `dev@56d6c3722`, one binary,
`/proc/loadavg` recorded on every run as this page requires — and the host was
BUSY (load 8–26), which matters in the direction noted at each row.

### 1. The cost this page trades away has already been repaid

`bbd9d05a9 fix(jit): name a direct call's staged argument oops in its safepoint
map` (2026-08-30, hours after the fix above) closed the dominant
`staged_unmappable` population. The trade this page documents — *"on
String-heavy code the fix stops relocation entirely"*, `compaction_cycles`
26 → 0 — does not reproduce:

| probe | `compaction_cycles` | `objects_relocated` | `relocation_skipped_jit` |
|---|---:|---:|---:|
| gate ON (default) | 13 | 70 032 | **0** |
| `CRATONVM_JIT_RELOC_GATE_ON_MAP_INCOMPLETE=0` | 13 | 70 014 | 0 |

Not one cycle declines. The named regression is gone with it:
`org.h2.test.store.TestMVStoreTool` at `--Xmx 1g` ran **clean to a 900 s cap in
both arms** (`oom=0`), against the 57–61 s OOM this page records.

So **`Next` items 2 and 3 are closed**: the follow-up that removes the cost
landed, and there is no longer a cost that would justify defaulting
`CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT` to OFF.

### 2. RETRACTED — `stale_live` is an upper bound, not a liveness proof

**The first version of this addendum claimed the gate leaves live stale words at
its shipped default. That claim is withdrawn: the measurement it rests on does
not support it, and the witness it named is a demonstrated false positive.**

`report_remap_residue` classifies a from-space word as LIVE by ONE test:

```rust
let class = if live_hi <= 0 { "unknown" }
            else if off < live_hi { "LIVE" }     // <- the whole test
            else { "dead" };
```

`live_frame_hi` is the **spill high-water mark**, not a liveness bound. Every
word below it is called LIVE, including a local slot the method has not written
yet, an `int` local whose 8-byte home still holds a previous frame's pointer,
and any spill slot below the watermark that is currently dead. The page already
learned the weaker form of this once — the raw `stale_words` count was an upper
bound until the `live_hi` split was added — and the split narrowed the bound
without turning it into a proof.

So the honest reading of the numbers below is "at most this many", not "this
many":

| arm | result | frames | words classified LIVE |
|---|---|---:|---:|
| gate ON (default) | clean to 900 s cap | 13 | ≤ 15 |
| `..._MAP_INCOMPLETE=0` | `rc=1` NPE at 497 s | 128 | ≤ 322 |

The gate-OFF arm still FAILS and the gate-ON arm still passes, which is real and
is the bisect this page already had. What is **not** established is that any
particular word in the gate-ON arm is a missed root.

### 3. The witness was dead storage, and the bytecode proves it

`probes/SafepointMapResidue.java` (25 lines, no H2, ~2 minutes) reliably reaches
the instrument and reports one stable frame:

```text
[remap-frame] method=java/lang/StringConcatHelper.doConcat:(...)
  sp_id=51 cov_complete=true live_hi=96
  mapped=[ 8=.. 16=.. 40=.. ] rewritten=1
  stale_words=18 stale_live=1 stale_dead=17
  [LIVE off=32 stale=0x20019013eb8->0x200102599b8]
```

Dumping the map inputs gave `local_mask=Some(19)`, and `local_offset(k)` is
`8*(k+1)`, so the named slots 8/16/40 are locals 0, 1 and 4. Offset 32 is
local 3. That looked like "the mask omits a live reference local".

**It is not. `javap -c` on the real method settles it:**

```text
25: istore_3          // local 3 = newLength — an INT
30: astore 4          // local 4 = buf (byte[]) — the reference
```

Bit 4 of the mask can only be set after `astore 4` at pc 30, which is after
`istore_3` at pc 25. So at every safepoint where the mask reads `Some(19)`,
local 3 holds an `int` — and the mask naming locals 0, 1 and 4 is **exactly
right**. The pointer sitting at offset 32 is stale bytes in a slot that does not
hold a reference at that program point: dead storage below the watermark, which
is the one thing the LIVE test cannot tell apart from a missed root.

**The lesson this page has now taught a third time.** Its own history is a WARN
read as a discriminator (wrong twice), then a raw residue count that was an
upper bound. `stale_live` is the same shape one refinement later. A residue
count cannot be evidence of a missed root without an independent statement of
what the slot HOLDS at that pc — and there is one available:
`docs/.../reference` on the verifier type maps makes exactly that point, and
`javap -c -l` scopes settle it by hand in a minute.


### 4. The cause census printed six of its seven causes — and the seventh is ZERO

`map_incomplete_cause::snapshot()` returns seven; `driver.rs` printed
`causes[0..=5]`. A method whose only unnameable references were inline-scope
locals therefore printed all-zero causes — "no cause", from a cause census.
**The `Which of the seven fired` section above was read off that line**, so the
column that was missing is precisely the one its conclusion could not have
ruled out. Fixed, with a `const` assert so a new variant is a compile error
rather than another silent column.

**Then the fixed census answered, and it is not the seventh cause either.** On
the witness method, with all seven columns printing:

```text
[oopcov] frameslot-detail method=java/lang/StringConcatHelper.doConcat:(...)
  precise_maps=true inline_sites=1 safepoints=11 mapped=11 unmapped_pcs=[]
  causes(marks_inexact=0 oop_in_reg=0 stack_deep=0 local_deep=0
         staged_deep=0 staged_unmappable=0 inline_local_unmappable=0)
```

That is worth having on its own: every safepoint of that method is mapped,
`unmapped_pcs` is empty, and no cause fires. An eighth counter
(`LOCAL_MASK_UNREACHED`, for the silent `None` branch where the locals are
skipped without setting `map_incomplete`) was added at the same time and also
reads zero.

**What that does NOT establish** — and the first version of this addendum said
it did — is that a live reference is going unnamed. All eight zeros are
consistent with the simpler reading, which §3 shows is the true one: the map is
right and the residue line is a slot that does not hold a reference at that pc.
A complete census reading zero on a correct map is what a correct map looks
like.

### 5. What is actually left, and what the next instrument has to be

The remaining question is unchanged from `Next` item 2, and it is now honestly
open rather than falsely answered:

* the gate-OFF arm reproduces the corruption; the gate-ON arm does not, over
  900 s at load 8–18. That is the bisect, and it stands.
* **no missed root has been exhibited under the shipped default.** Everything
  offered as one so far has been either dead storage (§3) or unverified.
* so the fail-closed gate may well be sufficient today, and the cost of keeping
  it is now nil (§1). That is a materially better position than this page
  describes, and it should not be undone on the strength of a residue count.

**Before any repair, the instrument needs to be able to say "live".** A word
below `live_frame_hi` that the map does not name is a missed root only if the
slot holds a reference at that bci. Two oracles exist in-tree for that and
neither is wired to this report:

* the **local-oop mask itself** — if the slot is a local, the mask already says
  whether it is a reference, and a LIVE classification that contradicts the mask
  is either a real miss or (as here) a slot that is not a live local at all;
* the **verifier type maps**, which this repo already records as the independent
  oracle for a never-mapped word.

Cross-checking the residue against either would have retired this witness in one
run instead of one commit. That, not another cause counter, is the next thing to
build.


### Where that leaves the page

* **OPEN**, but less alarmingly than it reads. The corruption reproduces with
  the gate OFF and not with it ON; the gate now costs nothing; and no missed
  root has been exhibited under the shipped default. `Next` item 2 stays open
  because nothing has PROVED the map complete — not because anything has shown
  it short.
* The next thing to build is an instrument that can say "live", not another
  cause counter. See §5.
* The three faces, the `ZGC_RELOCATE=0` bisect, the residue instrument and the
  fail-closed gate all stand as written.
* What must not be carried forward is the cost table and the
  `RELOCATE_UNDER_PROVEN_JIT` recommendation: both describe a binary that is two
  commits old.

## ADDENDUM 2026-09-02: the instrument that can say "live" now exists, and it says NO MISSED ROOT

§5 named the one thing to build before any repair: an instrument that can tell a
MISSED ROOT from DEAD STORAGE, because `stale_live` cannot. It is built, it is
committed, and it has been run on both this page's witness and this page's
workload. **`local_oop`, the only count that names a missed root, is ZERO on
both.**

### What was added

`OopMapEntry` now carries, per safepoint, the compiler's own answers about its
own frame — all of it diagnostic, nothing gates on it:

| field | question it answers |
|---|---|
| `local_oop_mask: Option<u64>` | is java local `k` a reference at this bci? `None` (dataflow never reached here, so the map named NO locals) is deliberately NOT the same value as `Some(0)` |
| `num_locals` | is this offset a java local at all, or past the band? |
| `inline_local_scopes: Vec<(base, n, mask)>` | the same, per live SPLICE — spliced locals come out of the operand-spill band, so the mask above cannot address them |
| `non_oop_stack_slots` + `stack_marks_exact` | did this safepoint's own operand-stack model classify that spill slot as a non-reference? Only spendable when the marks were exact — a padded mark vector is a default, not a proof |

`report_remap_residue` puts every stale word below `live_frame_hi` to those, in
order, and prints a VERDICT rather than adding to a count. `classify_stale_local`
is a pure function with 8 unit tests, one of which pins the retracted witness
(mask `Some(19)`, offset 32 → `local-not-oop`). `[remap-residue-summary]` at exit
carries the run totals, with `frames`, `frames_with_inline_scopes` and
`frames_with_stack_model` as ENGAGEMENT counters, because a zero from an
instrument that never fired is not a reading.

### The witness this page named: explained, in one run

`probes/SafepointMapResidue.java`, shipped default, ~2 minutes:

```
[remap-residue-summary] frames=28 frames_with_live_stale=1 local_oop=0
  local_not_oop=1 local_unreached=0 ... duplicate_of_mapped=0 mapped_alias=0
  outside_locals=0

[remap-frame] method=java/lang/StringConcatHelper.doConcat sp_id=51
  cov_complete=true live_hi=96 local_mask=Some(19) num_locals=5
  mapped=[ 8=.. 16=.. 40=0x20010200938 ] rewritten=1
  [LIVE off=32 k=3 local-not-oop region=java-local stale=0x20010256d18->0x20010200938]
```

Offset 32 is local 3; the mask names locals 0, 1 and 4 (offsets 8, 16, 40 —
exactly what the map holds); `javap -c` shows `25: istore_3`. The instrument now
states in one line what previously took a bytecode session to establish, which is
the whole point of §5.

### The workload this page is about: 59 of 61 explained, none of them a root

`org.h2.test.store.TestRandomMapOps`, `--Xmx 256m`, shipped default (gate ON),
1500 s cap, host at load 20, aggregated over the streamed per-frame lines:

| | |
|---|---:|
| frames reported | 64 |
| frames with a LIVE stale word | 50 |
| **`local_oop` (missed roots)** | **0** |
| `duplicate_of_mapped` | 55 |
| `mapped_alias` | 3 |
| `local_not_oop` | 1 |
| `outside_locals` (unexplained) | 2 |

No corruption, no exception, no OOM in the run.

**`duplicate_of_mapped` is the shape this page had been staring at.** The map
names a group of spill slots and the stale words are the copies a few slots
below them, holding the SAME objects at their pre-move addresses:

```
mapped=[ .. 232=0x2001027ec28 240=0x2001027ec98 248=.. 256=.. ]
[LIVE off=224 .. stale=0x20018c20218->0x2001027ec28]   <- same object as slot 232
[LIVE off=216 .. stale=0x20018c20288->0x2001027ec98]   <- same object as slot 240
```

The object is named, rewritten and not lost. What is left behind is an abandoned
copy — which is what "a live COPY of a reference in a frame word" always meant,
and it costs nothing.

### The instrument's own false positive, found and fixed in the same session

The first H2 run under the oracle reported `local_oop=1` — the tripwire firing.
It was wrong, and the line it printed proves it:

```
method=org/h2/mvstore/FileStore.readChunkFooter sp_id=49
  mapped=[ 8=0x20010461ee8 48=.. 56=.. 128=.. ] rewritten=4
  [LIVE off=8 k=0 LOCAL-OOP-UNMAPPED region=java-local stale=0x20010461ee8->0x20010418118]
```

`rewritten` equals the slot count, so **slot 8 was named and rewritten** — the
value sitting in it is one the rewrite had just written. It still answers to
`pointer_map.get()` because a slide moves objects into space other objects
vacated, so a TO-space address can alias another object's FROM-space address.

That is a property of the whole residue instrument, not of the oracle:
**"this word is a key of the pointer map" is not proof that the word is stale.**
It is now a verdict of its own (`mapped_alias`, 3 on the H2 run), the mapped
check runs before every other oracle, and a unit test pins it. Anyone reading an
older `stale_live` number should assume it contains this population too.

### What is NOT measured, stated plainly

* **The IR tier has no oracle.** Both remaining `outside_locals` words are in one
  frame, `FileStore.accountForRemovedPage`, whose line reads `local_mask=None
  num_locals=0` — `ir_lower` records no dataflow, so the oracle is silent by
  construction, not by measurement. Every unexplained word in the final run is
  there.
* **Two of the four oracles never fired.** `frames_with_inline_scopes=0` and
  `frames_with_stack_model=0` on every run: no reported frame had a safepoint
  INSIDE a splice, and none carried a frame-resident operand entry the model
  called a non-reference. Their zeros are engagement zeros and must not be read
  as findings. (A frame's `inlined=[..]` is the method list, not a live scope at
  that bci.)
* **Everything above is on a `livedbg` binary** (no LTO, opt-level 1). Not a
  preference: a release build was attempted twice and both times the fat-LTO
  link was OOM-killed (`signal: 9`), the second at `-j 1`, on a host reporting
  0–4 GiB available with 48 logged-in users. Map CONTENT is what is being
  measured and does not depend on how the VM itself was optimized, but a slower
  binary performs fewer operations per second, so "no corruption in 1500 s" is a
  weaker statement than the same wall clock on a release build.
* **The 4g arm was ATTEMPTED and is still unmeasured.** The 2026-08-30 L7
  addendum says the failure is not small-heap-only, so re-running `--Xmx 4g`
  against the shipped default is the other thing that would retire this page. It
  ran 22 minutes, reported 19 frames with `local_oop=0`, and then died `rc=137`
  — `dmesg` shows `oom_reaper: reaped process ... (cvm-mapor5-live)`. That is
  the HOST killing a 4 GiB heap on a box with ~5 GiB available and 48 logged-in
  users, not a VM defect and not a result. It needs a quiet host or a second
  machine.

### Where that leaves the page, 2026-09-02

**SUPERSEDED the same day — read the "(later)" addendum below.** The 4g arm WAS
re-measured (clean), and something worse turned up in its place: the NPE still
reproduces at the shipped default at 256m. The bullets below stand except for
the first, which claimed the 4g arm was the only thing left.

* Still **OPEN**, and the reason is no longer the 4g arm: it is that the defect
  itself still reproduces. Everything this page asked for as INSTRUMENTATION has
  been done.
* `Next` item 1 (the instrument) is **CLOSED** — built, tested, committed, and
  it answers the witness in one run.
* `Next` items 2 and 3 were closed by the 2026-09-01 addendum and stay closed.
* **Nothing in two workloads and four runs has exhibited a missed root at the
  shipped default.** That is now a measurement with engagement counters behind
  it rather than an absence of evidence.
* Read `local_oop` and `inline_local_oop`, not `stale_live`. The old number
  counts dead spill, abandoned copies of named roots, and to-space addresses
  that alias from-space keys — all three of which this session watched mislead a
  reader, twice including me.

## ADDENDUM 2026-09-02 (later): the defect is NOT fixed — it still reproduces at the shipped default

The addendum above concluded that the only thing keeping this page open was an
un-re-measured `--Xmx 4g` arm. **That is wrong, and the correction is the more
important half of the day.** The 4g arm was measured and is clean; what is not
clean is the heap size this page is named after.

All of the following is one RELEASE binary built from `dev@777688aa5`
(`cargo build --release`, fat LTO — the host finally had the memory for it),
`org.h2.test.store.TestRandomMapOps`, `/proc/loadavg` recorded on every run.

| arm | heap | runs | NPE | OOM | clean | times |
|---|---|---:|---:|---:|---:|---|
| **shipped default**, batch A | 256m | 3 | **2** | 0 | 1 | 866 s, 388 s |
| **shipped default**, batch B | 256m | 4 | 0 | 0 | 4 | 900 s cap |
| **shipped default**, long arm | 256m | 1 | 0 | 0 | 1 | 1500 s cap |
| shipped default | 1g | 1 | 0 | 0 | 1 | 1500 s cap |
| shipped default | 4g | 1 | 0 | 0 | 1 | 1500 s cap |
| `CRATONVM_ZGC_RELOCATE=0` | 256m | 4 | 0 | **4** | 0 | 296–573 s |
| `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` | 256m | 4 | 0 | **2** | 2 | 688 s, 860 s |

The failure is the `NullPointerException` face this page already documents,
with the same shape:

```
seed:-6609831345401105555 op:14 java.lang.NullPointerException
  at org.h2.mvstore.RandomAccessStore.readStoreHeader(RandomAccessStore.java:260)
  at org.h2.mvstore.FileStore.start(FileStore.java:944)
  at org.h2.mvstore.MVStore.<init>(MVStore.java:296)
```

### What this overturns

* **"The gate-ON arm does not reproduce, over 900 s at load 8–18" is withdrawn.**
  That was ONE run on a busy host. On a quiet host the same binary fails twice
  in three runs. This page's own 2026-08-29 section says the lever is host
  quietness and that a contended box hid the defect — the gate-ON arm was then
  measured on a contended box anyway.
* **The fail-closed gate reduced the rate; it did not close the hole.** Two
  failures in eight shipped-default runs today, against "9 in 9" before the fix.
  That is a real improvement and it is not a fix.
* **The 4g claim of the L7 addendum does not reproduce on current dev.** 1500 s
  clean at 4g and at 1g. The title's "small heap" premise is right again.

### What the batches do NOT support

Batch A failed 2 of 3 and batch B failed 0 of 4 on the SAME binary, same heap,
same workload. Batch B ran concurrently with the `RELOCATE=0` arm at loads
4.9–20.5. So the per-batch rate is not stable and **no rate quoted from a single
batch is worth anything** — including the two clean batches. Run them serially
on an idle host before believing any number here.

### The relocation lever still points the same way, and still has no clean control

Neither switch produces a clean arm at 256m: both remove the NPE and substitute
the fragmentation `OutOfMemoryError` this page's cost section describes —
`RELOCATE=0` in 4 of 4, the narrower `RELOCATE_UNDER_PROVEN_JIT=0` in 2 of 4.
Consistent with relocation being the lever, and **not a control**: an arm that
trades one failure for another cannot isolate either. The page has hit this
shape before, and the fix is a heap size where neither failure mode is forced —
which 1g and 4g are, and at which the NPE does not appear either.

### And the oracle says it is not a missed root

Across every run above, on ~370 reported frames spanning two heap sizes and both
binaries, **`local_oop` and `inline_local_oop` are ZERO**: not one frame word
below the live watermark that the compiler's own "must be oop" dataflow proves
is a reference and the map failed to name. The stale words are dead spill,
abandoned copies of roots the map DOES name, and to-space/from-space address
aliases.

That is a negative result and it is the useful kind. The hypothesis this page
has pursued since 2026-08-29 — *the map is short, relocation rewrites what it
names and leaves a live reference behind* — is not what the instrument built to
detect it finds. Either the defect is elsewhere in the relocation path (the
object header, the forwarding table, a non-frame root), or it is in a frame the
oracle cannot speak for. The next step is to widen the instrument to the
non-frame roots, not to keep looking for an unnamed local.

### Where that actually leaves the page

* **OPEN, and more open than the addendum above claimed.** The defect
  reproduces at the shipped default. Retiring it would have been wrong.
* The 4g/1g arms are closed: clean, release binary, 1500 s each.
* The next measurement is a SERIAL batch on an idle host — at least 10 runs at
  256m, nothing else on the machine — to get a base rate that a fix can be
  measured against. Every rate on this page so far was taken with something else
  running.
* The next INSTRUMENT is not another frame-word oracle. `local_oop=0` is now
  well-evidenced; look outside the compiled frame.

## ADDENDUM 2026-08-30 (L7 corpus lane): it is NOT a small-heap defect — 4g fails too

The `--jdk-only` corpus run hit this class at `--Xmx 1g` and could not attribute
it, which sent me here. Measuring the heap axis says the title's premise is too
narrow. Compatible mode, default collector, release binary, host at load 3-7:

```text
 256m   FAIL      38s   NullPointerException                       <- this page's case
   1g   FAIL     173s   AssertionError: Expected: 247 actual: 198
   1g   FAIL     181s   AssertionError: Expected: 57  actual: 55
   1g   FAIL     372s   NullPointerException
   1g   FAIL     480s   (the corpus run that started this)
   2g   TIMEOUT 1300s   no failure within the cap -- and NOT a pass
   4g   FAIL    1053s   AssertionError: Expected: 300 actual: 291
```

**The 256m row is a positive control and it reproduced**, so this is the same
binary and setup this page describes, not a different experiment.

Three things follow.

**1. Heap sets the LATENCY, not the occurrence.** 38 s at 256m, 173-480 s at 1g,
1053 s at 4g. Sixteen times the heap buys about twenty-five times the runway and
then it fails anyway. `2g` timing out at 1300 s fits that curve rather than
contradicting it — the run was still short of where 4g failed.

**2. At 1g and above the dominant face is a WRONG ANSWER, not a crash.**
`AssertionError: Expected: 247 actual: 198` at `TestRandomMapOps.testOps:162` is
the map reporting fewer entries than were put into it. Three of the six failures
above are that shape, at three different heaps and three different magnitudes
(247/198, 57/55, 300/291). Silent data loss is a worse failure mode than the
NPE this page opens with, and it is the one that scales UP with heap.

**3. There is still no passing CratonVM run at any heap.** `2g` did not pass; it
ran out of clock. So nothing here supplies the negative control this page's
`Next` section wants, and in particular:

**A caution about the `G30` coercion guard.** It fires in EVERY run above,
256m and 4g alike, ~21-24 log lines each. Its `occurrence=` values are exact
powers of two (131072, 262144, 524288) because the guard reports at doubling
intervals — that is its own sampling, not a severity measure. With no passing
run to compare against, neither its presence nor its counts discriminate
anything here, and I am recording it as an observation rather than a lead.

Everything below this section predates the addendum and is unchanged.

## The three faces

All on the post-`COLL-REFRESH`-fix binary, default collector (ZGC),
`--Xmx 256m`:

| rep | cap | outcome |
|---|---|---|
| 1 | 1300 s | clean to cap |
| 2 | 1300 s | clean to cap |
| 3 | — | **SIGSEGV at 155 s** — CLOSED 2026-09-05, see the top of this page |
| (earlier) | 1500 s | clean to cap |
| (earlier) | — | `NoSuchMethodError` at 845 s |
| (earlier, `dev`) | — | `NullPointerException: "d" is null` at 823 s |

```text
NoSuchMethodError: 'boolean <unknown class 2460030832>.equals(java.lang.Object)'
  at org/h2/test/store/TestRandomMapOps.assertEquals(TestRandomMapOps.java)
  at org/h2/test/store/TestRandomMapOps.testOps(TestRandomMapOps.java:162)
```

Two readings the parent page established, both worth having before spending
runs:

* **`2460030832` is not a class id.** It reads as a truncated pointer, so the
  cell had been REUSED, not merely zeroed. That places this at the opposite end
  of the stale-reference family from the G1 defect the parent page fixed, whose
  receiver was an all-zero header (`ClassId(0)` = `java.lang.Object`). The two
  ends need different questions: *who freed it* versus *who else allocated over
  it*.
* **The segfault's Java frames are not a location.** They name
  `TzdbZoneRulesProvider.load` / `BufferedInputStream.fill`, and the crash
  header says in as many words that frames are "published at the last
  blocking/safepoint deposit — may lag the faulting instruction". The registers
  (`rax=0x0000FFFFFFFFFFFC`, `r10=0xFFFFFFFFFFFFFB05`, unreadable) look like a
  length or index computed off a bad header, consistent with the other two faces
  and NOT with a timezone-loading defect.

`CRATONVM_DBG_COLL_REFRESH` reports **zero** engagements on the failing runs, so
the receiver-pinning path the parent page fixed is not involved at all.

## 2026-08-29: it is CHEAP now — 9/9 in 22–471 s, and the lever is a QUIET HOST

The page's own next move was *"make the defect cheaper before diagnosing it"*.
It is cheaper by about **thirty times**, and not because of any flag.

`--Xmx 256m`, 2026-08-29 tip, host load 2.7–4.7, three arms interleaved, three
reps each — all nine runs `rc=1`, all `oom=0 arena=0`, all
`NullPointerException`:

| arm | switches OFF | rep 1 | rep 2 | rep 3 |
|---|---|---:|---:|---:|
| `base` | — | 38 s | 22 s | 39 s |
| `novac` | `PUBLISH_VACATED` | 40 s | 471 s | 42 s |
| `neither` | all three | 322 s | 39 s | 41 s |

**9 of 9 failed, and no arm is distinguishable from any other.** So:

* **The 2026-08-29 collector repairs did not cause this and do not accelerate
  it.** That mattered enough to test: the vacated-span publication hands the
  slide's emptied space back to the allocator, which removes the leak that used
  to leave a stale holder facing a zeroed corpse — the parent work predicted in
  writing that the FACE of such defects would change. It does not change the
  RATE here. `neither` is the pre-change behaviour byte for byte and it fails
  just as reliably.
* **The lever is the host.** The base rate this page inherited — "roughly one
  failure in three runs of ~20 minutes" — was measured on the shared Azure box
  under load. At load ~3 it is 9/9 with a median around 40 s. A contended host
  was hiding a defect that reproduces almost every time, which is the mirror
  image of the trap the parent page documents (a quiet host hiding a race).
  **Any future arm on this class must record `/proc/loadavg` beside `rc`.**

One failing run's collector census, on a 256 MB heap, for scale:

```text
collections=30 compaction_cycles=24 objects_relocated=159832
relocation_skipped_jit=6 relocation_on_proven_jit=24
zgc-high-compaction: cycles=12 declined=12 vacated_spans=331
                     vacated_bytes=691546832
```

The failing seed and op are printed by the test itself
(`seed:-67298774724213935 op:1349`) and are, per this page's own history, not a
lever — but the stack is:

```text
Exception in thread "main" java/lang/NullPointerException
    at org/h2/test/store/TestRandomMapOps.openStore(TestRandomMapOps.java)
    at org/h2/test/store/TestRandomMapOps.testOps(TestRandomMapOps.java:90)
```

**This is now a defect somebody can bisect in a lunch break**, which is exactly
what the section below was waiting for. `CRATONVM_ZGC_RELOCATE=0` is the first
arm to run: it restores non-moving behaviour byte for byte, so a failure that
survives it is not a relocation defect at all.

## 2026-08-29 (later): the first arm this page names has been RUN — it IS a relocation defect

`CRATONVM_ZGC_RELOCATE=0` was this page's own prescribed first arm, on the rule
that *"a failure that survives it is not a relocation defect at all"*. It does
not survive it.

Azure Linux, `--Xmx 256m`, 900 s cap, interleaved base/norelo, one binary
(`dev@a94842f04`), load recorded on every run as this page requires:

| arm | rep | rc | secs | load0 | `oom` | `arena` | signature |
|---|---|---:|---:|---:|---:|---:|---|
| base | 1 | 1 | **122** | 23.1 | 0 | 0 | `NullPointerException` |
| `ZGC_RELOCATE=0` | 1 | 124 (cap) | 900 | 17.1 | 0 | 10 | — none — |
| base | 2 | 1 | **786** | 21.3 | 0 | 0 | `NullPointerException` |
| `ZGC_RELOCATE=0` | 2 | 124 (cap) | 900 | 19.9 | 0 | 9 | — none — |
| base | 3 | 1 | **64** | 12.0 | 0 | 0 | `NullPointerException` |
| `ZGC_RELOCATE=0` | 3 | 124 (cap) | 900 | 16.0 | 0 | 10 | — none — |

**base 3/3 fail; `ZGC_RELOCATE=0` 3/3 clean to the cap.** The cap is
7-14x the base median, so a clean arm here carries information by this page's
own standard.

The `arena` column is the confirmation that the switch ENGAGED rather than
silently doing nothing: with relocation off the arena fragments and reports
9-10 allocation failures, which is exactly what relocation exists to prevent.
A clean arm with `arena=0` would have meant the flag was inert.

So the defect is in relocation, and the remaining question is *which* relocation
obligation is unmet. `relocate_stw`'s own doc names the shortlist and says the
audit is unfinished:

> The returned `PointerMap` is **non-empty** ... Every consumer of a raw heap
> address outside this heap — JIT frame maps, monitor tables, external root
> providers, native side tables — must be remapped through it ... **Auditing
> those arms is the reason this stays behind a default-off flag**

It is no longer behind a default-off flag. `CRATONVM_ZGC_RELOCATE` and
`CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT` both default ON.

### What was checked and came back CLEAN

The native side-table arm of that shortlist was audited by diffing every
`gc_scan_*` root provider against its remap counterpart across
`native-builtins`, `native-io`, `native-collections`, `native-api`,
`native-awt` and the crypto/security crates. **32 scans, 32 updates, all
paired and all wired post-GC.** Four looked unpaired at first
(`gc_scan_selector_roots`, `gc_scan_channel_roots`,
`gc_scan_ssc_socket_cache_roots`, `gc_scan_ss_back_ref_roots`) — they use the
other naming convention, `*_update_after_gc`, and are called. That is a
negative result, and it removes the cheapest hypothesis rather than
supporting it.

### It narrows once more, and then the instrument names it

`CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` — **2/2 clean to the 900 s cap**
(`arena=10`, `arena=8`). That switch restores the older refusal that fires on
the mere existence of a compiled frame, so for a JIT-active workload it is
close to `ZGC_RELOCATE=0`; the value of the arm is that it places the defect in
**relocation while a compiled frame is live**, not in relocation generally.

`CRATONVM_DBG_REMAP_RESIDUE=1` on a failing base run then says what is wrong
with those frames. The instrument walks a live JIT frame looking for words that
are still **keys** in the pointer map — pre-move addresses nothing rewrote —
and one failing run (`rc=1`, 286 s, the usual `NullPointerException`) reports:

```text
rep 1 (rc=1, 286 s)      frames=235  with_stale=228 (97%)
                           cov_complete TRUE=228  false=0
                           mapped slots on stale frames: min=1 max=13 avg=4.5
                           stale_words: min=1 max=33 total=2844
rep 2 (rc=1,  99 s)      frames=54   with_stale=54 (100%)
                           cov_complete TRUE=54   false=0
                           mapped slots on stale frames: min=1 max=11 avg=5.2
                           stale_words: min=1 max=36 total=701
```

**Across two independent failing runs, every single frame that still held a
pre-move address had declared `cov_complete=true` — 282 of them, and not one
reporting incomplete coverage.** A representative line:

```text
[remap-frame] method=java/lang/StringLatin1.newString:([BII)Ljava/lang/String;
  sp_id=7 frame_size=912 cov_complete=true
  mapped=[ 8=0x20012279570] rewritten=1
  inlined=["java/lang/String.<init>([BB)V"]
  stale_words=12 [off=888 stale=0x200137528b8->0x20012279848]
                 [off=688 stale=0x20013752590->0x20012279520] ...
```

A 912-byte frame, an oop map naming **one** slot, one slot rewritten — and
twelve other words in that same frame holding addresses the collector has a
forwarding entry for. The relocation moved those objects; the frame kept the
old pointers; a later read through one of them is the `NullPointerException`,
and reading through a slot whose old cell has since been REUSED is this page's
`NoSuchMethodError: '<unknown class 2460030832>'` and its SIGSEGV. **Three
faces, one cause.**

So the defect is not "the heap is fragmented" and not the allocator: **the
per-frame coverage proof reports complete while the frame demonstrably retains
unrewritten references, and relocation trusts it.**

Inlining is present in only 45 of 228 and 9 of 54 (20% and 17%), so a spliced
callee's unnamed locals are *a* contributor and not the whole of it — the average stale frame
names 4.5 slots and carries several more live from-addresses than that.

**The caveat that count needed has now been paid off, and it mattered.**
`stale_words` counted every frame word that is a pointer-map key, and the
spill cursor *reclaims by moving, it does not clear* — so a from-space address
above `live_frame_hi` is DEAD storage, not a missed root. The instrument now
splits them (`stale_live` / `stale_dead` / `stale_unknown`, using the
`live_frame_hi` the band verifier already reads). Re-measured:

| rep | rc | secs | frames | frames with a **LIVE** stale word | **LIVE** words | dead words | `cov_complete` on stale frames |
|---|---:|---:|---:|---:|---:|---:|---|
| 1 | 1 | 472 | 344 | **248 (72%)** | **761** | 3 559 | TRUE=342, false=0 |
| 2 | 1 | 72 | 74 | **56 (76%)** | **159** | 721 | TRUE=74, false=0 |

So roughly five in six of the original 2 844 were reclaimed spill and are
correctly ignorable — **and 761 + 159 were not.** Those are references inside
the live band, holding addresses the collection has a forwarding entry for,
that nothing rewrote. Every frame carrying one declared complete coverage;
across both runs, 416 stale frames and not one `cov_complete=false`.

### The minimal witness: `String.substring(II)`, safepoint 41

The same frame appears in both runs, byte for byte apart from the addresses:

```text
[remap-frame] method=java/lang/String.substring:(II)Ljava/lang/String;
  sp_id=41 frame_size=752 cov_complete=true live_hi=120
  mapped=[ 8=0x20012279540 ] rewritten=1 inlined=[]
  stale_words=7 stale_live=2 stale_dead=5
  [LIVE off=112 stale=0x200137525e0->0x20012279590]
  [LIVE off=88  stale=0x200137525e0->0x20012279590]
```

The live band is `off < 120`. The map names **one** slot — offset 8, the
`this` parameter home — and rewrites it. Offsets **88 and 112 are inside that
band**, both hold the *same* reference, and the collector has a forwarding
entry for it. They are left pointing at the pre-move address.

**`inlined=[]`.** No splice, no unnamed callee locals. That retires the
hypothesis this page reached for first: it is not a spliced callee's locals,
it is a plain method whose own live operand slots are not in its own map.

`FileStore.submitOrRun` makes the same point from the other side:

```text
mapped=[ 8=0x0 16=0x20012282400 104=0x20012282400 ] rewritten=2 live_hi=112
stale_live=1 [LIVE off=96 stale=0x2001ad221b0->0x20012282400]
```

Slots 16 and 104 were named, rewritten, and now hold the NEW address. Slot 96
— also inside the live band — still holds the OLD address of that same object.
The map found two homes of one reference and missed a third.

### What is actually wrong

`ir_lower::emit_safepoint_map` builds `slots` from exactly two sources: the
reference PARAMETER homes, and every `IrType::Ref` node that has a slot. It
sets `coverable = false` only when a `Ref` node has no slot or an unencodable
offset. **A live copy of a reference sitting in an operand-spill slot that is
not a `Ref` node's own home is in neither source, and its absence does not
clear `coverable`.** The map is therefore complete with respect to what the
lowerer enumerates and incomplete with respect to what the frame holds — and
`moving_young_coverage_complete` is the flag relocation is gated on.

The contract block above that function states obligation 3 as
*"`frame_slot_offsets` naming every frame slot that holds a live reference"*.
That is the obligation not being met; the code checks a narrower property and
reports it under the wider name.

## 2026-08-30: FIXED (fail-closed), with a measured cost and a named follow-up

`jit/src/x64/safepoint.rs` maintains `map_incomplete` in seven places while
building a safepoint's slot list — an oop still in a register, a stack, local,
staged-argument or inline-local offset that will not fit `i16`, inexact stack
marks, and *"a reference staged somewhere no map can name it ... **Fail
closed**"*. It fed exactly one consumer: `mapped_safepoint_pcs`, i.e.
`fully_oop_covered`, i.e. whether the CONSERVATIVE backstop stays on.

**It never reached `moving_young_coverage_complete`, which is the flag
relocation is gated on.** That was sound while the backstop was the whole
story — a conservative sweep MARKS what the map missed, so nothing is lost
when nothing moves. Relocation must REWRITE the slot, and a conservative scan
cannot. So a map this function had already judged short went to the collector
labelled complete, and `remap_one_jit_frame` rewrote what it named and left
the rest pointing into from-space. The author's stated fail-closed intent was
never wired to the gate; the fix is that wire, extracted as a pure
`relocation_coverage_complete(shadow_complete, map_incomplete)` with a truth
table, a can-only-subtract property, and a source witness that
`map_incomplete` actually reaches the push.

### Which of the seven fired

`CRATONVM_DBG_OOPCOV=1` on a 40-line reproducer:

```text
causes(marks_inexact=0 oop_in_reg=0 stack_deep=0 local_deep=0
       staged_deep=0 staged_unmappable=19)
frameslot-detail method=java/lang/String.substring:(II)…
       safepoints=6 mapped=3 unmapped_pcs=[1, 28, 41] … staged_unmappable=13
```

**One cause, `staged_unmappable`** — a reference staged into the native-ABI
outgoing-argument area, a direct-call service slot, or an inlined callee's
parameter locals. 13 of the 19 in `substring(II)` alone, whose unmapped
safepoints `[1, 28, 41]` contain safepoint 41 — the witness this page already
had.

### Verification

`TestRandomMapOps`, `--Xmx 256m`, 900 s cap, fixed binary and the `dev` binary
**interleaved on the same host in the same window**:

| pair | fixed | `dev` |
|---|---|---|
| 1 | **clean, 900 s**, `arena=0` | `rc=1` **NullPointerException at 286 s** |
| 2 | **clean, 900 s**, `arena=2` | `rc=1` **`AssertionError: Expected: 221 actual: 214` at 77 s** |
| 3 | **clean, 900 s**, `arena=0` | `rc=1` **NullPointerException at 50 s** |
| (4th fixed run) | **clean, 900 s**, `arena=2` | — |

**Fixed 4/4 clean across 3 600 s; `dev` 3/3 failed in 413 s combined.**

Pair 2 is worth its own line: `Expected: 221 actual: 214` is a **silent wrong
answer**, a fourth face beyond the three this page lists. A stale reference
that happens to land on a valid-but-wrong object does not crash.

### The cost, measured rather than asserted

The same probe, `CRATONVM_GC_STATS=1`, both binaries, identical workload:

| arm | `compaction_cycles` | `objects_relocated` | `relocation_skipped_jit` |
|---|---:|---:|---:|
| `dev` | 26 | 145 | 0 |
| fixed | **0** | **0** | **26** |

**On String-heavy code the fix stops relocation entirely** — every cycle that
meets a live compiled frame now declines. That is a real loss of the
defragmentation the arena depends on, taken in exchange for not corrupting the
heap, and it is the trade the codebase's fail-closed discipline prescribes.

It is *not* as blunt as `CRATONVM_ZGC_RELOCATE=0`: on H2 the fixed arm held
`arena=0/2/0/2` where the wholesale switch showed `arena=8/9/10`. The likely
reason is that this declines only cycles that meet a live compiled frame with
a short map, while the switch declines all of them — **not directly measured**,
and worth confirming before it is repeated as fact.

### The regression this causes, and the switch that exists for it

That cost lands somewhere real. `org.h2.test.store.TestMVStoreTool` is an
already-open ZGC fragmentation OOM, and it now arrives about **ten times
sooner**:

| arm | rep 1 | rep 2 |
|---|---|---|
| `dev` | `rc=1` at **581 s** | — |
| fixed | `rc=1` at **61 s** | `rc=1` at **57 s** |

Same failure either way (`OutOfMemoryError: Java heap space`, `anewarray`), so
this is not a new defect — it is the known one reached faster, exactly as the
engagement census predicts once relocation stops defragmenting.

So the coupling ships behind **`CRATONVM_JIT_RELOC_GATE_ON_MAP_INCOMPLETE`**,
default ON, `=0` restoring the pre-fix behaviour in one binary. Default ON
because publishing a map the compiler has already judged short is heap
corruption and the wrong-answer face is silent; a switch rather than a
constant because the trade is real, because the ZGC lane needs something to
bisect against, and because whoever hits the fragmentation side needs a lever
that is not "turn off relocation entirely".

### The switch, verified in both directions

One binary, one workload, the flag the only difference:

| `CRATONVM_JIT_RELOC_GATE_ON_MAP_INCOMPLETE` | `compaction_cycles` | `objects_relocated` | `relocation_skipped_jit` | **LIVE stale words** |
|---|---:|---:|---:|---:|
| ON (default) | 0 | 0 | 26 | **0** |
| `=0` (pre-fix) | 26 | 154 | 0 | **33** |

That is the causal chain end to end and in a single binary: turn the gate off,
relocation runs under short maps and live stale words come back; turn it on,
they are gone and relocation declines instead. It also means neither arm of
this page's trade can be claimed without the other being measurable.

### The follow-up that removes the cost

All three `staged_unmappable` sites are the same shape in
`x64/bytecode_walk.rs` (7631, 8083, 10506): after `pop_invoke_args(n)`, if any
popped argument is an oop, declare the staging unmappable. But the comment
immediately above one of them records that *"every `arg_slots` entry stays
live until `emit_stack_arg_setup` marshals it into the entry ABI far below"* —
so in that window the references sit in nameable frame slots, and the existing
Stage-3 `pending_staged_arg_oops` mechanism already knows how to name slots by
offset. Naming them there would restore relocation without restoring the
defect. The hazard, and the reason it is not done here: after marshalling, the
live copy is in the outgoing ABI area and the original slot is dead, so naming
it past that point would have the collector rewrite a word that no longer owns
the reference — trading this defect for its mirror image.

### Next, in order

1. **`String.substring(II)` at safepoint 41 is a one-method reproducer.** It
   is deterministic across runs and needs no H2 — a unit test that compiles it,
   relocates under a live frame and asserts `stale_live == 0` would fail today
   and is the regression test this repair wants.
2. **Repair direction, and why it is not a one-liner.** Either the lowerer
   names every live-band slot holding a reference (it does not know about
   copies it did not allocate), or `coverable` is cleared whenever it cannot
   prove it did — which is the fail-closed direction the surrounding code
   already prefers and which would cost relocation on most frames until the
   first option lands.
3. **Consider defaulting `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT` to OFF in the
   meantime.** `relocate_stw`'s own doc says the consumer audit is *"the reason
   this stays behind a default-off flag"*; it no longer is, and the proof it
   relies on is measurably unsound. That is a project call, not a drive-by:
   it trades this corruption for the fragmentation the arena column shows
   (`arena=8-10` whenever relocation stops), and another lane is actively
   working the peer-side half of the same coverage question.

## Why "repro and dump" WAS the wrong instrument

One failure in three, twenty minutes a run, and a different symptom each time
means an attempt costs an hour and buys a signature nobody has seen before.
A clean arm shorter than the base rate says nothing — the same trap the parent
page documents for the `ClassCastException` and the reason its recorded seed was
retired.

**The base rate has to come down in cost before a kill-switch bisect means
anything**, because only then does a clean arm carry information.

## Reproducing

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 256m \
    -c "$CP" org.h2.test.store.TestRandomMapOps
```

`CRATONVM_DBG_CCE_BT=1` dumps the offending receiver's shape, frame stack and
move history at the dispatch miss. `CRATONVM_DBG_COLL_REFRESH=1` counts receiver
moves the native-collection pinning absorbs — it is what proved the G1 defect
fixed and what proves this one is a different site.

`org.h2.test.store.SeededRandomMapOps` (added 2026-08-24) reflects into the
private `testOps(String,int,long)` so a seed can be pinned; it prints
`SEEDED_PASS`/`SEEDED_FAIL` per rep and exits non-zero on a failure, so it is
usable as a bisect target once one exists. It does not reproduce this row —
the failure is a GC/JIT schedule, not a function of the operation sequence.

## Related

- `fixed-suite-bugs/h2-suite-bugs/bug-h2-testrandommapops-classcastexception-20260821-RETIRED-20260829.md`
  — the parent page, with the G1 fix and the eleven-run non-reproduction.
- `docs/known-issues/gc/G30-1-the-silent-reference-slot-coercion-20260817.md`
  — the WARN family this may or may not belong to. The parent page's own
  history is that reading that WARN as a discriminator was wrong twice.
- `docs/known-issues/h2/correctness-issues-consolidated.md` — indexes this
  alongside the rest of the 48-class union's correctness results.
