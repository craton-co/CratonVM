# The ZGC relocation slides wrote into granules the give-back had returned

## Status

**FIXED 2026-09-04.** Retired from
`bug-box-unbox-intrinsic-segv-under-relocation-20260902`, which named it "the box/unbox intrinsic SIGSEGVs under a relocating
collector". It is neither the intrinsic's fault nor relocation's, and the
intrinsic is default-ON again.

## What the page had, and what was right about it

`org.h2.test.store.TestRandomMapOps --Xmx 256m`, family on: **SIGSEGV, 11/11**
at 25–183 s when the page was written, and **3/3 in 7 s** once the unrelated
`op:1033` wrong answer stopped ending the run first. Fault address always a
page boundary, `rdi` equal to it, fault pc inside libc.

That description is exact, and it is the whole diagnosis. A `memmove` whose
destination register *is* the faulting page boundary is a copy that ran off the
end of a mapping — not a read through a stale reference, which is what the
page inferred.

## The switch table that misled, and the third switch

The page had two switches that each removed the crash:

| arm | SIGSEGV |
|---|---|
| shipped default (family on) | **11 / 11** |
| `CRATONVM_ZGC_RELOCATE=0` | 0 / 3 |
| `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` | 0 / 3 |

and read them as *"it takes BOTH relocation and this intrinsic"*. A later pass
added `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` (0/3) and concluded the stale
value was a reference in a live compiled frame that relocation moved without
rewriting — "a root the safepoint's oop map does not name".

There is a third switch, and it is under both of the first two:

| arm | SIGSEGV | note |
|---|---|---|
| `CRATONVM_JIT=box-unbox-intrinsic` | **3 / 3**, 7 s | positive control, same batch |
| ...`+ CRATONVM_GC_RESERVE=0` | **0 / 3** | |

`CRATONVM_GC_RESERVE=0` forces the wholly-committed `alloc_zeroed` backing
store. It has nothing to do with the JIT and nothing to do with oop maps.

**The lesson: a pair of switches that each remove a crash identifies two
ingredients, not a mechanism.** Both of the page's switches are downstream of
the real one — relocation is what performs the copy, and the intrinsic is what
changes the allocation shape enough to make the *high* slide run at all.

## Root cause

gdb, on the 7-second repro:

```
#0 __memcpy_avx512_unaligned_erms
#1 core::ptr::copy<u8>
#2 cratonvm_gc::zgc::…::compact_high_region   gc/src/zgc.rs:5462
#3 cratonvm_gc::zgc::…::relocate_stw          gc/src/zgc.rs:6409
```

and `info proc mappings` at the fault shows `---p` runs *inside* the arena's
own reservation.

`Arena::decommit_free_blocks` returns free-list granules to the OS. Its doc
gave the reason that was safe:

> every path that hands one out again goes through `hand_out`, which commits
> before it returns a pointer

Both of this collector's slides are a path that hands one out and does **not**.
A slide picks its destination inside free space arithmetically and `memmove`s
into it — never asking the allocator for anything is the whole point of a
slide. So the first slide after a give-back writes into a `PROT_NONE` granule.

The high slide is the one that fires here because it packs survivors against
`capacity`, and the top of the arena is exactly where a give-back has most
recently been. The low slide has the identical exposure.

## The fix

`Arena::commit_for_relocation` is the door such a caller needs, and both slides
now go through it. A refused commit is not a reason to write anyway: the high
slide treats the survivor as immovable, the low slide takes its existing
"nowhere below it, so it stays put" branch. A slide that cannot have its
destination is a missed compaction, never a corrupt heap.

The stale promise in `decommit_free_blocks`'s doc is corrected in the same
change — it is the sentence that made this look safe.

## The intrinsic is default-ON again

The mitigation existed for this crash and nothing else. With the slides fixed:

| arm | runs | SIGSEGV |
|---|---:|---:|
| `CRATONVM_JIT=box-unbox-intrinsic`, 1200 s cap | 3 | **0** |
| `CRATONVM_JIT=box-unbox-intrinsic`, 400 s cap | 3 | **0** |

What those runs end on instead is a `NullPointerException` at a **later** seed
(`seed:4604578705726772870`), and it appears identically with the family OFF —
so it is not this page's defect. It is the pre-existing failure
`known-issues/h2/bug-h2-testrandommapops-small-heap-corruption-20260829.md`
records, which the page itself listed as pre-dating its bisect range.

`box_unbox_is_opt_in_until_the_relocation_defect_is_closed` asked to be
inverted when this page closed, "so the flip has to be deliberate". It is
inverted and renamed.

## What this retires from the page, item by item

| the page said | now |
|---|---|
| "it takes BOTH relocation and this intrinsic" | Both are ingredients; the cause is the uncommitted destination. |
| "a root the safepoint's oop map does not name" | No stale root is involved. `RELOCATE_UNDER_PROVEN_JIT=0` worked by removing the slide, not by fixing a root. |
| "the receiver must already be stale when it is LOADED" | It is not. The fault is a WRITE, in the collector, with no mutator frame involved. |
| bisect first-bad `a910b7d9c`, a merge with two good parents | Consistent: the give-back and the slide arrived from different lines. |
| "the repro is currently BLOCKED by an earlier failure" (`op:1033`) | Cleared — see `guarded-inline-native-screen-asked-the-declaring-class-FIXED-20260904.md`. |
| `probes/BoxUnboxReloc.java` / `…MT.java` found nothing over 5M relocations | Correct, and now explained: neither drives the HIGH slide, which needs large-object allocation and a give-back. |

That last row is worth keeping. Both probes were carefully built, their
ingredients were measured as engaged, and they were still blind — because the
ingredient list came from the hypothesis. `objects_relocated=3.2M` proves
relocation ran; it says nothing about `compact_high_region`, which has its own
counter (`high_compaction_cycles`) that neither probe read.

## A second workload, and a chase that ran on a stale binary (2026-09-04, later)

`org.h2.test.db.TestLargeBlob` reproduces this page's defect, and it was
independently chased for most of an evening as if it were a NEW bug, because
the binary under test predated the fix above.

The reproducer, for anyone who wants a second workload for this family:

```bash
JDK25=/data/toolchain/jdk-25 CRATONVM_BIN=<binary> CRATONVM_ZGC_ALLOC_TRIGGER=25 \
  ./run-h2-suite.sh run --category all --start 33 --count 1 --tag lb
```

`CRATONVM_ZGC_ALLOC_TRIGGER=25` is what makes it work: on this class it is the
difference between **0 collections and 34**. Nothing ever collected there
before, so nothing ever slid.

The A/B, interleaved round by round on one host, `b011f0dc0` (which predates
`19854a573`) against dev tip:

| arm | SIGSEGV |
|---|---|
| pre-fix binary | 2 / 5 — and 5 / 7 in an earlier batch, so **7 / 12** |
| dev tip | **0 / 8** |

Same signature both times: a libc `memcpy`, `rdi` at or one AVX store below a
2 MiB-aligned fault address, `rbp` holding a 23.9 MB length.

### The two inferences that kept the chase going, both wrong

**"`bytes_copied == occupancy`, so relocation never ran."** That counter is the
SWEEP shard's — it accumulates the size of every live object the complement
pass measures, so of course it equals occupancy. The high slide's copies are
counted by `high_bytes_copied`, which the pause line does not print. Exactly
the trap the row above this section already records for the two probes: reading
a relocation question off a counter that does not answer it.

**"The GC summary for cycle 20 printed, then the crash, so the mutator did
it."** Cycle 20's summary printing means cycle 20 finished. Cycle 21's slide
crashes before printing anything. A summary line is evidence about the cycle it
names and about no other.

The general form: `git log` between the tip and whatever built the binary you
are running, BEFORE building a theory on its behaviour.

### What came out of it anyway

`reservation::recent_decommit_covering` — a 64-slot ring of the spans this
process handed back to the OS, printed by the crash handler when the fault
address is inside one, tagged with the give-back's site. This page reached
`compact_high_region` through gdb; the crash report said nothing. With the ring
armed the same crash prints `site=free-list-high` and "NOT re-committed since",
which is the diagnosis, in the report, from one run.

### Confirmed on the full H2 suite, 2026-09-05

Not just on the reproducer. All 218 classes, ZGC, `jit-real`, three shards on
`vm1`, against the 2026-09-03 ZGC full run taken under comparable contention:

| | baseline (09-03) | dev tip (09-05) |
|---|---|---|
| PASS | 176 | **182** |
| FAIL | 16 | 15 |
| HANG | 21 | 21 |
| **CRASH** | **5** | **0** |

**Zero regressions** — no class that passed on 09-03 stopped passing. And no
`SIGSEGV` or fatal-error report anywhere in the run; the baseline had five, and
all five are gone:

| class | 09-03 | 09-05 |
|---|---|---|
| `db.TestLargeBlob` | CRASH | **PASS** |
| `db.TestLIRSMemoryConsumption` | CRASH | **PASS** |
| `unit.TestPerfectHash` | CRASH | **PASS** |
| `synth.TestPowerOffFs2` | CRASH | FAIL |
| `store.TestKillProcessWhileWriting` | CRASH | HANG |

**`TestKillProcessWhileWriting`'s HANG is the SUITE'S CAP, not a failure**
(established 2026-09-06). That class's healthy runtime is 605-716 s under G1 and
811 s under the default collector, and `run-h2-suite.sh` caps a class at
`CLASS_TO=300`. It reports HANG whatever the VM does. See
`known-issues/h2/bug-h2-testkillprocesswhilewriting-g1-oom-20260829.md`, which
carries the cap finding and the `CLASS_TO=1800` invocation. The suspicion below
that these are "300 s class timeouts on a shared box" was right about the number
and one step short of the cause: the cap is below the measurement even on an
idle host.

The last two still do not pass, but they no longer crash — a different failure,
to be taken on its own terms rather than as this one.

Worth recording about `TestLargeBlob` specifically: it crashed on 09-03 with NO
`CRATONVM_ZGC_ALLOC_TRIGGER` set. The trigger was a way to make the defect
reproduce on demand, not a precondition for it — the suite was meeting this by
default, on three classes, and one of them (`TestLIRSMemoryConsumption`) had its
own known-issue page pointed elsewhere.

The remaining movement is HANG/FAIL churn among classes that were already
failing (`TestCluster` and `TestDiskFull` to PASS, `TestStringCache` HANG to
PASS, `TestKill` FAIL to HANG, `TestMVStoreCachePerformance` HANG to FAIL).
HANG totals are identical at 21, and these are 300 s class timeouts on a shared
box, so read them as contention rather than as signal.
