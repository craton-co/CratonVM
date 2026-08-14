# ZGC Phase 1 — the empirical baseline, re-established from data already in the tree

**Written 2026-08-13.** This is Phase 1 of
[`zgc-maturity-assessment-and-plan-20260813.md`](zgc-maturity-assessment-and-plan-20260813.md),
which asked for "a current three-way table" and warned that the default flip
rested on "one Tomcat run and a Spring Boot number that is five days stale".

**No suite was re-run for this page.** Every number below was already recorded
somewhere in this repository; the work was finding it, dating it, and putting
the columns beside each other. That constraint turned out to matter, because
the single most useful record had been **deleted from the tree three days after
it was written**, leaving six pages citing a path that no longer existed.

---

## 1. The headline: on the freshest data, the Tomcat margin is one class

`docs/gc-tuning.md` and `docs/GC.md` both justify the default flip with this:

> ZGC 604 PASS / 29 HANG / 0 CRASH in 247 min against Generational's
> 519 / 115 / 1 in 356 min.

That is the **2026-08-10** row of the three-way comparison. The same record
re-ran all three backends in the identical shape **the next day**, and printed
this:

| 651-class Tomcat suite | PASS | FAIL | HANG | CRASH | wall |
|---|---:|---:|---:|---:|---:|
| Generational 08-10 | 519 | 16 | 115 | 0 | 356.5 min |
| G1 08-10 | 578 | 35 | 34 | 4 | 267.0 min |
| ZGC 08-10 | 604 | 18 | 29 | 0 | 247.1 min |
| **Generational 08-11** | **628** | 12 | **11** | 0 | **177.5 min** |
| **G1 08-11** | **623** | 7 | 20 | 1 -> **0** (note) | 212.9 min |
| **ZGC 08-11** | **629** | 11 | **11** | 0 | **178.4 min** |

(note) the 08-11 G1 crash was a fault inside the `CRATONVM_DBG=g1-dbg-reach`
verifier, which ran on the G1 arm only; flag off, the class passes 3/3.

**ZGC's lead over Generational on 2026-08-11 is 1 class and 0.9 minutes**, not
85 classes and 109 minutes. The gap the shipping documentation quotes closed
the day after it was measured, and the record says so in its own words:
*"Whatever was costing the default collector 100 hangs was fixed in that
window, not by anything on this page. Treat single-day cross-backend gaps here
as perishable."*

**This does not overturn the flip.** ZGC is still the top row on PASS, still
tied for fewest hangs, and still the only backend that has never crashed on
this suite. What it overturns is the *size* of the claim: the documentation is
quoting a margin that its own source superseded, and Phase 1 existed precisely
to catch that.

---

## 2. Widening past Tomcat: four more suites already have multi-collector data

The maturity plan asked for Tomcat and Spring Boot. Four other suites had
already been run under multiple collectors, and they are the stronger evidence
because they are independent workloads with different allocation shapes.

| suite | classes | Generational | G1 | ZGC | date | source (internal unless noted) |
|---|---:|---|---|---|---|---|
| **Spring Framework** | 2848 | 2819 OK / 18 FAIL / 11 TIMEOUT | 2824 / 20 / 4 | **2823 / 20 / 5** | 08-10 | `spring/gc-variant-fullsuite-classpath-gap-and-fails-20260810-FIXED.md` |
| **Tomcat** | 651 | 628 / 12 / 11 HANG | 623 / 7 / 20 | **629 / 11 / 11** | 08-11 | `known-issues/tomcat/gc-backend-3way-fullsuite-comparison-20260810.md` (public) |
| **H2** | 65 (rerun union) | 19 PASS / 0 CRASH | 19 / **4 CRASH** | **19 / 0 CRASH** | 08-10 | `h2-suite-bugs/gc-variant-fullsuite-crashes-hangs-fails-20260810-FIXED.md` |
| **Hibernate Reactive** | 12 (batch 02) | 12/12 | 12/12 | **12/12** | 08-12 | `hibernate-reactive/investigate-batch-02-CLEARED-20260812.md` (four collectors) |
| **Spring Boot** | 1975 | 1902 PASS / 18 HANG | — | **1860 / 49** | **08-08** | `springboot/zgc-real-fullsuite-regression-RETIRED-20260808.md` |

Read together:

* **On four of the five suites ZGC is at or above parity.** Spring Framework is
  a 1-class difference in 2848 (0.04%); Tomcat is +1; H2 and Hibernate Reactive
  are exact ties — and H2 is the one suite where a collector *did* separate
  itself, that being G1, with 4 crashes the other two did not have.
* **The one suite where ZGC is behind is the one whose measurement is oldest.**

---

## 3. Spring Boot: Phase 1 cannot be closed from existing data, and that is the finding

The Spring Boot row is 42 classes behind Generational, and it is the number
`gc-tuning.md` has been carrying. Three facts about it, all already recorded,
all pointing the same way:

1. **It is dated 2026-08-08** — the oldest number in the table by four days.
2. **It was measured on a binary without two ZGC-only fixes**, both landed
   2026-08-10: a missing reference-array un-box and a stale generated-`$ProxyN`
   cache. `gc-tuning.md` already flags the row as "stale in ZGC's disfavour".
3. **The first of those two defects is exactly the shape that manufactures
   spurious FAILs.** `TestMessageFactory` on the Tomcat suite failed under ZGC
   only, because `ChoiceFormat`'s `double[]` of thresholds came back all-zero;
   every branch then matched and the last one won. A defect that silently
   zeroes a primitive array does not fail in one place — it fails wherever that
   shape occurs, which is unbounded until re-measured.

**No suite was re-run for this page**, and no Spring Boot full-suite run under
ZGC exists after 08-08 anywhere in `apps/spring-boot-suite-runner`. The
freshest artifact there is a five-class residual rerun (2026-08-12,
`RESULTS-20260812-failhang-rerun.md`: 4 PASS, 1 environment-gated EMPTY, zero
failures), which is a residual check and not a baseline.

Phase 1's exit criterion is therefore **partially met**, and the honest
statement is:

> ZGC is at parity or better on every suite measured since 2026-08-10. The
> single suite where it is behind has not been measured since 2026-08-08, on a
> binary missing two of its own fixes, one of which repaired a defect that
> silently zeroes primitive arrays. **That row should be treated as unmeasured,
> not as evidence against ZGC** — and re-running it is the one piece of Phase 1
> that still requires a machine.

---

## 3b. CORRECTION, later the same day: the Spring Boot answer was in the tree

Section 3 above says Phase 1 "cannot be closed from existing data" and that no
Spring Boot full-suite run under ZGC exists after 2026-08-08. **The first half
is wrong and the second is true but irrelevant.**

There is no post-08-08 *full-suite* run. There is something better suited to
the question: on **2026-08-10**, after the two ZGC-only defects were fixed, the
**26 classes that were the entire ZGC-vs-default delta** were re-run on one
binary, `-XX:+UseZGC` vs the default, `-Xmx 2g`, 300s/class.

| arm | PASS | HANG | FAIL |
|---|---:|---:|---:|
| **ZGC** | **16** | 7 | 3 |
| default (Generational) | 14 | 10 | 2 |

The record's own verdict: *"No functional ZGC-vs-default difference is left."*
Five rows move across the timeout boundary, three of them in ZGC's favour, and
the one that goes the other way (`BatchJdbcAutoConfigurationTests`) passes on
both arms when run alone. Raw arms:
`CratonVM-zgcres-20260809/apps/spring-boot-suite-runner/.suite/results/zgcres-final-{zgc,default}-20260810/`.

**Why this is parity and not a sample.** Those 26 classes are by construction
the complete set on which the two arms differed in the 1975-class suite; the
other ~1,949 already agreed. "No difference left across the delta set" is
therefore a statement about the whole suite.

**What I got wrong, and it is the mistake this very page was written to catch.**
Section 1 shows `gc-tuning.md` quoting a Tomcat margin its own source had
superseded the next day. I then did the same thing with 1860-vs-1902 — quoted
the 08-08 pre-fix figure as the live one, four times, while the run that
superseded it sat in a sibling worktree's runner folder. Searching for a *new*
measurement is not the same as asking whether the old one still stands.

## 4. The plan's own warning cut both ways, in the same table

The maturity plan said: *"If ZGC is not at pass-rate parity with Generational,
that is the finding and the default should be revisited — the comparison page's
own warning that single-day cross-backend gaps are perishable cuts both ways."*

It did, in both directions at once:

* **Against the documentation's headline** — the 85-class Tomcat margin was
  perishable and did not survive a single day.
* **Against the case for reverting** — the 42-class Spring Boot deficit is
  perishable in exactly the same way, is older, and has a named mechanism that
  was subsequently fixed.

So the conclusion is neither "the flip was wrong" nor "the flip is vindicated".
It is that **ZGC and Generational are within single-digit classes of each other
on every current measurement, and the collectors are separated by their failure
*shapes*, not by their pass rates**:

| collector | its worst allocation shape | evidence |
|---|---|---|
| Generational | a single array larger than a semi-space | `TestCharChunkLargeHeap`: a ~2 GB `char[]` at `-Xmx 2g`. G1 and ZGC both serve it and pass in ~6.4 s |
| ZGC | many large buffers over a long life, with no compaction to recover the gaps between survivors | `ZipContentTests` OOMs at 2g, passes at 3g |
| G1 | (correctness, not shape) root coverage at relocation time | the only backend to have crashed on any of these suites |

That table is the real Phase 1 result, and it is more useful than a pass-rate
ranking, because it tells an operator which collector their workload wants
rather than which one won a suite on one day.

---

## 5. What this changes in the shipping documentation

* `gc-tuning.md` and `GC.md` now quote the **08-11** Tomcat row beside the
  08-10 one, instead of the 08-10 row alone.
* The three-way comparison record is **restored** to
  `docs/known-issues/tomcat/`, with a note recording that `ad3393f7c` deleted
  it and left six pages citing a dead path.
* The 1.5x heap premium in `gc-tuning.md` stays flagged as **never
  re-measured**. It comes from one class (`ZipContentTests`, 2g -> 3g), and the
  2026-08-13 Tomcat OOM that looked like the same shape turned out to be mostly
  a TLAB reservation bug instead. One data point that has since been partly
  re-attributed is not a sizing constant.

## 6. Not done here

* **The Spring Boot 1975-class suite under ZGC on current `dev`** — the one
  measurement Phase 1 genuinely still owes; see section 3.
* **Isolated (non-concurrent) reruns.** All three Tomcat arms shared the host
  with each other by design, which keeps the comparison fair but makes the
  absolute wall times unusable as throughput figures.
* **A ZGC arm for Keycloak, Netty, Elasticsearch and Quarkus.** Those runners
  exist and have no per-collector sweep recorded.
