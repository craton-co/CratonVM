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
frames=235  with_stale=228 (97%)
  cov_complete TRUE=228   cov_complete=false 0
  mapped slots on stale frames: min=1 max=13 avg=4.5
  stale_words: min=1 max=33 total=2844
```

**Every one of the 228 frames that still held a pre-move address had declared
`cov_complete=true`.** Not one reported incomplete coverage. A representative
line:

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

Inlining is present in only 45 of the 228 (20%), so a spliced callee's unnamed
locals are *a* contributor and not the whole of it — the average stale frame
names 4.5 slots and carries several more live from-addresses than that.

**Caveat, stated because the count is a heuristic**: `stale_words` counts frame
words whose value is a pointer-map key. A dead slot or a spilled non-reference
integer that happens to equal a moved object's old address would be counted
too. At 228 frames of 235 and 2 844 words that is not a coincidence budget
anyone can spend, but the *exact* count is an upper bound, not a proof of 2 844
live misses.

### Next, in order

1. **Find why `coverage_complete` is true here.** It is the claim that is
   demonstrably false, and it is what relocation is gated on. The
   `scauses(...)` census on the sibling ZGC page
   (`dataflow=151 marks=83 inline_scope=5`) is the compile-time half of the
   same question.
2. Whether the map is *incomplete* or the *rewrite* skips slots it named —
   `mapped=[…] rewritten=N` versus `stale_words` in one line separates those,
   and above they disagree in the same frame.

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
