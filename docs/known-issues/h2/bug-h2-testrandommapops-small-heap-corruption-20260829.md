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

## The three faces

All on the post-`COLL-REFRESH`-fix binary, default collector (ZGC),
`--Xmx 256m`:

| rep | cap | outcome |
|---|---|---|
| 1 | 1300 s | clean to cap |
| 2 | 1300 s | clean to cap |
| 3 | — | **SIGSEGV at 155 s** |
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
