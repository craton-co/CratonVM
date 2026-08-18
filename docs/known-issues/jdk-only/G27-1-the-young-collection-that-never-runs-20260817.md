# G27-1 — the young collection that never runs

**Status:** MEASURED, end to end. Every number below was taken on this host with
the shipping release binary; nothing here is PREDICTED. **No Rust file was
changed by this lane** — see §8 for why, and §7 for the costed plan that
replaces the change.

| | |
|---|---|
| binary | `C:/craton/target-fcheck/release/cratonvm.exe`, release, mtime 2026-08-17 00:44 |
| attributable? | **partly**, exactly as `G20-1` says: this lane did not build it either, and did not re-run the `find -newermt` check `HANDOFF-20260814` §3 prescribes. Every measurement is of *that file*. |
| oracle | HotSpot 25.0.3+9-LTS, `C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot` |
| host | this Windows 11 box, 32 logical CPUs, **several agents running concurrently throughout** |
| workload | `bench/BinTreesClassic.java` (checksum `68332206`), `bench/HashMapOnly.java`, plus two scratchpad probes described in §9 |
| totals | **96 bintrees runs + 54 hashmap runs + 18 regression-vector runs. 0 checksum mismatches, 0 vector failures.** |
| time basis | **wall throughout.** "kernel ms" is the benchmark's own in-process `System.currentTimeMillis()` span, which is also wall. **No CPU time is claimed anywhere in this record.** |

Arms are interleaved *within* a round with the order rotated by round index.
Load moved by more than 2x within this session (the same `-Xmx8g` ZGC arm read
12,424 ms and 28,356 ms in the same 8-round series), so **no number in one table
may be compared with a number in another** — compare within a row.

---

## 0. The headline

`G20-1` §4 named the largest measured defect in the VM as "the moving-young path
is *requested* and never runs". The premise is right and the attribution is
wrong, and the correction is bigger than the original finding.

| | |
|---|---|
| why `moving_young: cycles=0` | **the moving-young path is a branch inside `gc/src/gen_heap.rs`, and `gen_heap.rs` is not the collector in a default run.** The default has been ZGC since 2026-08-10 (`vm/src/config.rs:769`). `gc/src/zgc.rs` contains **zero** occurrences of the string `moving_young`. |
| is the predicate broken? | **No.** Select the generational backend and the same workload runs **8/8 cycles MOVING**, `coverage_fallbacks=0`, reason `moving-jit-coverage-proven`. |
| what the default costs | bintrees d=18, medians of 12 interleaved rounds: **`-Xmx1g` ZGC 25,108 ms vs Generational 3,956 ms — 6.35x**, checksum-identical, non-overlapping ranges. |
| is the 1.22 s pause about the live set? | **No, and this is now measured directly.** At `-Xmx256m` the registered set is *identical* on all 19 steady-state cycles (4,172,672) while the live set ranges over **2.9x** (535,498 → 1,551,271) and the pause does not follow it. |
| is the gap only GC? | **No.** At `-Xmx8g` both backends run exactly **one** GC cycle (ZGC pause 73 ms) and ZGC is still **4.37x** slower. That residue is the allocator, which belongs to the collector. |
| is Generational simply better? | **No — measured counterexample in §5.3.** On `HashMapOnly 8000000` it is faster in the kernel and **slower in process wall**. |

**The one-line correction to `G20-1` §4: there is no gate waiting for anything.
The counter is reporting on a collector that is not running.**

---

## 1. The gating predicate, named

### 1.1 What actually gates a moving young collection

**`gc/src/gen_heap.rs:5719`**, inside `GenerationalHeap::collect_garbage_inner`:

```rust
let moving_young = moving_young_requested && !divert_for_incomplete_moving_coverage;
```

fed by `gen_heap.rs:5703` (`moving_young_requested`), `5715` and `5717`, and
consumed by `divert_non_moving` at `5720`. The non-moving arm returns at
`gen_heap.rs:5779` (`run_non_moving_young_cycle`); the moving arm counts itself
at `gen_heap.rs:5787` (`record_moving_young_cycle`).

**This predicate is not what is failing.** §2.3 shows it evaluating MOVING on
every cycle of the exact workload `G20-1` measured.

### 1.2 What it is *waiting for* — nothing; it is never reached

`GenerationalHeap::collect_garbage_inner` is only called when the selected
backend is `GcAlgorithm::Generational`. The default is not:

* `vm/src/config.rs:769` — `#[cfg(feature = "zgc")] gc_algorithm: GcAlgorithm::Zgc`.
  The comment above it dates the change to 2026-08-10 and gives the evidence
  (651-class Tomcat suite: ZGC 604 PASS / 29 HANG / 0 CRASH against
  Generational's 519 / 115 / 1).
* `vm/src/vm/vm_init.rs:1718` dispatches that to the ZGC backend.
* `grep -n "moving_young" gc/src/zgc.rs` → **no matches.**

### 1.3 Why the report says `moving_young_requested=true`

`gc_metrics.rs:915` sets `FLAG_MOVING_YOUNG_REQUESTED` from
`gc_quiescence::moving_young_enabled()` (`gc/src/gc_quiescence.rs:231`). That
function's own doc comment says what it means:

> When on, **`gen_heap::collect_garbage_inner`** runs the moving (Cheney) young
> collection even while JIT frames are live […] Reads what the VM published from
> the codegen gate.

It is a **JIT-codegen capability flag** — "does compiled code publish a
rewritable precise root map" — and it is consulted unconditionally, by every
backend, whether or not that backend has a moving young generation at all. It
is not a request for a collection, and `true` there says nothing about whether
one was declined.

### 1.4 Why the report says "no collection has run yet" after five collections

`gc_metrics.rs:973` prints that string when `last_collector_decision()` returns
`None`, i.e. when `record_collector_decision` has never been called
(`sequence == 0`). Only `gen_heap.rs` and `g1.rs` call it. **ZGC never records a
decision**, so on a default run the line is literally true of the decision
register and grossly misleading about the process — which had, in `G20-1`'s own
transcript, just completed five collections costing 5.4 seconds.

**Three independent diagnostics all say "generational" on a run where no
generational code executed**, and a lane reading them carefully drew the wrong
conclusion. That is a defect in the instrument, and it is nominated in §6 N2.

### 1.5 The repository already knew, in two places `G20-1` did not read

* `regression-suite/harness-guard.sh:637-641` — "Without the flag it runs on the
  default (**ZGC since 2026-08-10**)".
* `gc/src/zgc.rs:2244-2251` — the `registry` field's own doc: "`bench/BinTreesClassic.java`
  measured **this backend at 6.6x/11.3x/18.4x the generational collector** at
  bt12/14/16".

`G20-1` §8 item 3 listed "why `zgc-*` is the label on the default collector's
counters … may be nothing more than a legacy counter prefix. It was not chased."
**It was not a prefix. It was the collector, and chasing it was the whole
finding.** This is the reason that item mattered.

---

## 2. The A/B — MEASURED

### 2.1 Method

```bash
export JAVA_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
CV="C:/craton/target-fcheck/release/cratonvm.exe"
cd bench                       # BinTreesClassic.class, javac'd by the oracle
# one phase per fresh process; three arms; order rotated 3 ways by round index
"$CV" --java-home "$JAVA_HOME" -Xmx$H --verbose:gc                        -cp . BinTreesClassic 18
"$CV" --java-home "$JAVA_HOME" -Xmx$H --verbose:gc -XX:+UseGenerationalGC -cp . BinTreesClassic 18
"$JAVA_HOME/bin/java"          -Xmx$H                                     -cp . BinTreesClassic 18
```

Wall from `date +%s%N` either side of the process; kernel from the benchmark's
own print. Medians, **no sample discarded**, checksum read from every run.

### 2.2 Wall time, fresh process

| heap | arm | n | **median** | min | p25 | p75 | max |
|---|---|---:|---:|---:|---:|---:|---:|
| `-Xmx8g` | CratonVM default (ZGC) | 8 | **15,303** | 12,424 | 13,686 | 21,639 | 28,356 |
| `-Xmx8g` | `-XX:+UseGenerationalGC` | 8 | **3,502** | 3,237 | 3,289 | 3,889 | 3,944 |
| `-Xmx8g` | HotSpot | 8 | 546 | 503 | 536 | 640 | 899 |
| `-Xmx1g` | CratonVM default (ZGC) | 12 | **25,108** | 23,124 | 23,880 | 28,167 | 34,965 |
| `-Xmx1g` | `-XX:+UseGenerationalGC` | 12 | **3,956** | 3,499 | 3,881 | 4,306 | 6,172 |
| `-Xmx1g` | HotSpot | 12 | 606 | 561 | 573 | 652 | 724 |
| `-Xmx256m` | CratonVM default (ZGC) | 12 | **26,616** | 22,145 | 25,480 | 30,614 | 32,610 |
| `-Xmx256m` | `-XX:+UseGenerationalGC` | 12 | **5,320** | 4,349 | 5,140 | 6,082 | 6,436 |
| `-Xmx256m` | HotSpot | 12 | 540 | 504 | 526 | 595 | 868 |

In-process kernel ms, same runs: 8g **14,830 / 3,098 / 411**; 1g **24,556 /
3,547 / 460**; 256m **26,222 / 4,938 / 416**.

| heap | ZGC / Generational | Generational / HotSpot | ZGC / HotSpot |
|---|---:|---:|---:|
| 8g | **4.37x** | 6.4x | 28.0x |
| 1g | **6.35x** | 6.5x | 41.4x |
| 256m | **5.00x** | 9.9x | 49.3x |

**At every heap size the ZGC arm's *minimum* is larger than the Generational
arm's *maximum*** (12,424 > 3,944; 23,124 > 6,172; 22,145 > 6,436). On a host
whose load moved 2x within the series, that is the form of the claim that
survives: the ranges do not touch.

**All 96 runs produced checksum `68332206`. Zero mismatches.**

### 2.3 The counters, from the same runs

| heap | ZGC cycles | ZGC total pause (median) | Generational cycles | of which MOVING | coverage fallbacks |
|---|---:|---:|---:|---:|---:|
| 8g | 1 | 0.068 s | 1 | **1** | 0 |
| 1g | 5 | 6.35 s | 8 (10 runs) / 9 (2 runs) | **8 / 9** | 0 |
| 256m | 20 | 7.60 s | 35 | **34** | 1 |

Verbatim, `-Xmx1g`, `-XX:+UseGenerationalGC`:

```
[GC] generational: minor=8 major=0
[GC] decision histogram: moving=8 non_moving=0 moving-jit-coverage-proven=8
[GC] decision #8: backend=generational young=MOVING reason=moving-jit-coverage-proven
                  (moving_young_requested=true jit_active=true unregistered_jit_frame=false)
[GC] moving_young: cycles=8 coverage_fallbacks=0
```

Eight moving young collections, **under a live JIT**, with a proven root map and
no fallback. The predicate of §1.1 works.

---

## 3. What one ZGC cycle is actually a function of — MEASURED

### 3.1 The discriminating reading

One instrumented run, `-Xmx256m`, reading the VM's own per-cycle lines. The
registered set is pinned by the heap size, so the live set is the only thing
that varies:

```bash
"$CV" --java-home "$JAVA_HOME" -Xmx256m --verbose:gc -cp . BinTreesClassic 18
```

| cycle | `registered` | `objects_copied` (live) | `pause_us` | ns per registered object |
|---:|---:|---:|---:|---:|
| 2 | 4,172,672 | 536,204 | 305,600 | 73.2 |
| 8 | 4,172,672 | 535,963 | 263,102 | 63.1 |
| 13 | 4,172,672 | 593,018 | 247,797 | 59.4 |
| 16 | 4,172,672 | 764,832 | 303,570 | 72.8 |
| 18 | 4,172,672 | 1,289,104 | 268,055 | 64.2 |
| 19 | 4,172,672 | 1,551,271 | 348,606 | 83.5 |
| 20 | 4,172,672 | 1,551,203 | 337,436 | 80.9 |

Over cycles 2–20 the **live set spans 2.90x** (535,498 → 1,551,271) and the
**pause spans 1.54x** with no monotone relationship to it — cycle 8 has a third
of cycle 19's live set and 75% of its pause. The `registered` column is constant
to the object.

The same shape at `-Xmx1g` (registered 16,755,584 on every steady-state cycle;
live 535,745 → 1,223,707, a 2.28x span; pause 1,084,193 → 1,537,849 µs):
**65–96 ns per registered object.**

**The pause is a function of everything allocated since the last cycle, not of
what survived.** `G20-1`'s 73 ns/object figure reproduces, at two heap sizes,
and is now shown to be per-*registered*-object by construction rather than by
inference.

### 3.2 Why — ten touches per dead object, in `gc/src/zgc.rs`

`ZgcRealHeap::collect_garbage` (`zgc.rs:8917`) does the following to **each of
the ~16.2 M objects that are about to be reclaimed**:

| # | site | work |
|---|---|---|
| 1 | `zgc.rs:8974` → `bases()` `zgc.rs:2166` | push into a `Vec<usize>` created with **`Vec::with_capacity(self.extra.len())`, which is `with_capacity(0)` on the bitmap arm** — the arm every default run takes. 16.7 M pushes then grow geometrically. |
| 2 | `zgc.rs:8978` | mark-bit clear pass: `header_mut(base).clear_gc_flags()` — a read-modify-write over ~800 MB |
| 3 | `zgc.rs:9303` | sweep pass: second full traversal, header read + `alloc_size` |
| 4 | `zgc.rs:9333` | `write_bytes(base, 0, size)` — 778 MB of memset at `-Xmx1g` |
| 5 | `zgc.rs:9335` | `arena.add_free_block(..)` — one small-bucket `Vec` push **per object** |
| 6 | `zgc.rs:9338` | `dead.push(base)` — a third unreserved `Vec` |
| 7 | `zgc.rs:9357` → `arena.rs:1431` | `coalesce_free_list()` → `low_blocks_sorted()` (`arena.rs:1775`) collects **16.2 M `(usize, usize)` tuples ≈ 260 MB** |
| 8 | `arena.rs:397` | `sort_by_offset` — 3-pass radix sort with a `vec![(0,0); 16.2M]` scratch, ≈ 1.5 GB of traffic |
| 9 | `arena.rs:1789` | `clear_low_free_list()` re-sums all 16.2 M sizes |
| 10 | `zgc.rs:9435` | `registry.remove(*d)` per dead base |

Plus one term that is **O(heap capacity, not occupancy)**: `registry.snapshot()`
(`zgc.rs:8973` → `2096`) copies one `u64` per 512 arena bytes — 16.8 MB at
`-Xmx1g`, **134 MB at `-Xmx8g`, every cycle, whatever the occupancy**. The 8g
run's single cycle costs **73,465 µs against only 28,361 registered objects**
(2,590 ns/object); that is the fixed term with nothing else in it, and it is the
cleanest isolation of it available without a rebuild.

A Cheney young collection (`gen_heap.rs:5812` onward) does **none** of this. It
copies the live set to to-space and resets from-space's cursor. Its cost is
O(live). That is the whole of the 6.35x.

### 3.3 The `-Xmx8g` retention claim reproduces exactly

```
[GC] zgc-real: collections=1 occupancy=3281498344/8589934592 bytes
```

3,281,498,344 B, byte-identical to `G20-1` §4.1. `G20-1`'s reading of it stands:
at the default heap the VM is fast because it does not collect. What is new is
that the generational backend at the same `-Xmx8g` also runs exactly **one**
cycle and is still 4.37x faster — so retention is not buying ZGC parity, it is
only hiding the sweep.

---

## 4. The prior attempt, and how this differs

`BENCHMARK.md` (Binary Trees bullet) records a 512 MiB young-semispace cap that
measured as a **12–13% regression** and was reverted.
`internal/performance/binarytrees-bt18-half-gap-20260730.md` gives the
mechanism in full: the cap forced ~6x more young collections, and *every* young
collection on that workload was falling back to the non-moving sweep
(fallback reasons `missing-exact-rbp`,
`innermost-rbp-belongs-to-unguarded-callee`). Its "Remaining gap" section says:

> the biggest remaining lever […] is the pre-existing defect where moving-young
> almost never actually moves for this workload […] If that were fixed, a
> smaller young semispace would likely become a net win again.

**How this lane differs, in one sentence: the prior attempt changed young
*sizing* on the premise that moving-young never moves, and this lane measured
that the premise is no longer true and changed nothing.**

MEASURED, on this binary, `-XX:+UseGenerationalGC`, same benchmark, same depth:

| heap | moving young cycles | non-moving fallbacks | fallback rate |
|---|---:|---:|---:|
| 8g | 1 | 0 | 0% |
| 1g | 8 | 0 | 0% |
| 256m | 34 | 1 | 2.9% |

The July 2026 "always falls back" behaviour is **gone**. The July record's own
stated precondition for revisiting the semispace cap is therefore satisfied —
and that is a `gen_heap.rs` change this lane deliberately did not attempt (§8).

---

## 5. Three further results, one of which cuts against the finding

### 5.1 The GC-sensitive vectors pass under both collectors — MEASURED

All nine, **with their real flags**, `--jdk-only`, replicating `run.sh`'s
per-vector logic (rc check, crash grep, `extract()` filter, cross-VM key diff
against HotSpot). The runner is a scratchpad copy; `regression-suite/run.sh` was
not invoked, and the repo's `regression-suite/build` was not written to (the
vectors were compiled to the scratchpad — only 3 of the 9 were prebuilt there).

| vector | flags used | default (ZGC) | `-XX:+UseGenerationalGC` |
|---|---|---|---|
| `RJitGc` | — | PASS (1 CK) | PASS (1 CK) |
| `RMapGcStress` | — | PASS (1 CK) | PASS (1 CK) |
| `RMapResizeGc` | — | PASS (1 CK) | PASS (1 CK) |
| `RForNameGcStress` | — | PASS (1 CK) | PASS (1 CK) |
| `ROverlaySystemGcStress` | — | PASS (1 CK) | PASS (1 CK) |
| `RPriorityQueueGc` | `--nojit --Xmx 64m` | PASS (3 CK) | PASS (3 CK) |
| `RTreeRangeGc` | `--Xmx 64m` | PASS (4 CK) | PASS (4 CK) |
| `RClassUnloadSweep` | — | PASS (1 CK) | PASS (1 CK) |
| `RClassUnloadSweepGen` | `-XX:+UseGenerationalGC` | PASS (1 CK) | PASS (1 CK) |

**18/18 PASS, every one cross-VM identical to HotSpot, oracle rc=0 on all nine.**
This is a baseline, not a claim about a change — nothing was changed.

### 5.2 hashmap is not a GC row at all — MEASURED

`HashMapOnly` at `-Xmx1g`, 10 interleaved rounds, checksum `15499991500000` on
all 30 runs:

| arm | n | wall median | kernel median | **GC cycles** |
|---|---:|---:|---:|---:|
| CratonVM default (ZGC) | 10 | 906 | 592 | **0** |
| `-XX:+UseGenerationalGC` | 10 | 639 | 325 | **0** |
| HotSpot | 10 | 188 | 71 | n/a |

**Zero collections in every run of both arms**, and still 1.82x between the two
backends. `G20-1` §2's second-worst row (hashmap, 10.5x) therefore has **no GC
component at this size** — the backend difference there is entirely allocator.
Repeated at n=8,000,000: still 0 ZGC cycles, 1 generational cycle.

### 5.3 The counterexample — Generational is *not* uniformly faster

`HashMapOnly 8000000`, `-Xmx1g`, 8 interleaved rounds, checksum
`991999932000000` on all 24 runs:

| arm | wall median | kernel median |
|---|---:|---:|
| CratonVM default (ZGC) | **5,544** | 5,168 |
| `-XX:+UseGenerationalGC` | **7,451** | 3,744 |
| HotSpot | 608 | 456 |

Generational is **27% faster in the kernel and 34% slower in process wall.** A
probe that stamps epoch millis at `main` entry and exit (`HmPhase`, §9) splits
the difference:

| arm | total | startup | kernel | in-`main`-outside-kernel | shutdown |
|---|---:|---:|---:|---:|---:|
| Generational | 10,243 | 266 | 4,247 | **5,494** | 236 |
| ZGC | 7,582 | 292 | 7,138 | **0** | 152 |

Under Generational, ~3.5–5.5 s is spent inside `main` after the kernel returns —
with the counters showing only `minor=1 major=0`, one MOVING cycle,
`moving-no-jit-frames-live`. **Cause NOT ESTABLISHED.** It is recorded because
it is a real cost on a real workload and because it is exactly the shape of
thing that becomes a HANG in a 651-class suite. **Any proposal to change the
default collector must explain this row first.**

---

## 6. NOMINATIONS — everything outside `gc/src/gen_heap.rs` and `gc/src/zgc.rs`

### N1 — the `--help` text says the default is Generational; it is ZGC
**File:** `vm-cli/src/main.rs:587-593` (the `--XX:UseGc` clap doc). It reads
"CratonVM honours `G1` and the default `Generational`; any other collector warns
and falls back to Generational." Every clause is wrong on this build:
`vm/src/config.rs:769` defaults to `Zgc`, and `config.rs:51` parses `z`/`zgc`.
**Risk: NONE** (documentation string). **Value: this sentence is one of the two
things that produced `G20-1` §4's misattribution**, and it is the one an
operator reads.

### N2 — the collector-decision report is silent about which collector ran
**File:** `gc/src/gc_metrics.rs:960-985` (`collector_decision_report`).
**MEASURED symptom:** a default `--verbose:gc` run that completed five
collections prints
`[GC] decision: no collection has run yet (moving_young_requested=true)` beside
`[GC] moving_young: cycles=0` and a full block of `[GC] cards: …` /
`[GC] young_sweep: …` generational counters that are structurally zero because
no generational code ran.
**Fix:** print the active backend unconditionally, and make the `None` arm say
"the active backend (`zgc`) does not record a decision" rather than "no
collection has run yet". The generational-only counter blocks should be
suppressed, or labelled, when the backend is not generational.
**Risk: LOW** (diagnostic output; note `gc_metrics.rs:1502` asserts on the
current string and would need updating). **Value: HIGH.** This instrument cost a
careful lane its central conclusion, and it is the instrument every future GC
investigation will start from.

### N3 — re-take the ZGC-vs-Generational default decision, with these numbers in it
**File:** `vm/src/config.rs:769`. **This lane does NOT recommend flipping it.**
The comment there records the reason it was flipped (Tomcat 604/29/0 against
519/115/1) and that reason is a liveness argument, which outranks throughput.
What is new is the price, which was never costed: **4.4x–6.4x on
allocation-bound work at every heap size**, plus 3.28 GB retained at the default
heap. §5.3 is the counterweight and must be resolved first.
**Owner:** whoever owns the Tomcat suite. **Risk: HIGH.**

### N4 — `Arena::coalesce_free_list` is O(dead objects · log) every cycle
**File:** `gc/src/arena.rs:1431`, `1775` (`low_blocks_sorted`), `397`
(`sort_by_offset`), `1789` (`clear_low_free_list`).
**Measured context:** §3.2. At `-Xmx1g` this sorts ~16.2 M entries per cycle to
produce a handful of merged spans.
**Preferred fix is P2 in §7, which lives in `zgc.rs` and needs no change here.**
Nominated only as the fallback if P2 is rejected: cap the low free list, or make
coalescing incremental.
**Risk: MEDIUM** — this arena is shared with `gen_heap.rs`'s non-moving sweep.

### N5 — `G20-1` §4 needs an amendment, and `BENCHMARK.md` a dated note
`G20-1` §4.3's "the generational moving-young path is requested and never runs"
and N4's "root-cause *why* moving-young is requested and declined" both rest on
the misattribution corrected in §1. `BENCHMARK.md`'s "young GC always falls back
to non-moving sweep for this workload" is a July-2026 observation that §4 above
measures as **no longer true** (0 fallbacks at 8g and 1g, 1 in 35 at 256m).
Neither document is this lane's to edit.

---

## 7. The costed plan — what to change in `gc/src/zgc.rs`, and what it will not buy

**Nothing here was applied.** §8 says why.

### P1 — reserve the `bases()` vector (≈4 lines, risk NONE)
`zgc.rs:2166` builds the cycle's base list with `Vec::with_capacity(self.extra.len())`,
which is **0** on the bitmap arm — the arm every default run takes, since
`zgc_start_bits_enabled_by_default()` (`zgc.rs:1592`) is on unless
`CRATONVM_ZGC_STARTBITS=0`. 16.7 M pushes then reallocate geometrically: ~24
reallocations and ~268 MB of `memcpy` per cycle. One popcount pass over `words`
(2.1 M `count_ones` at `-Xmx1g`, sub-millisecond) sizes it exactly.
`zgc.rs:2080` (`ZObjectStarts::bases`, used by `walk_objects` and the
`CRATONVM_ZGC_RELOCATE` path) has the same `Vec::new()` and the same fix.
**Pure capacity hint. No behavioural change is possible.**

### P2 — free-list one span per RUN of adjacent dead objects (≈25 lines, risk MEDIUM)
In the sweep at `zgc.rs:9303-9340`, `all` is **ascending** — `bases()`'s own doc
comment at `zgc.rs:2161-2165` states it and states the intent: "adjacent dead objects
hand adjacent spans to `Arena::add_free_block`, which is what the post-sweep
coalescer merges." Do the merge at the source: carry `(run_start, run_end)`,
extend it while `base == run_end`, and emit **one** `add_free_block` when the run
breaks. On bintrees this collapses ~16.2 M small-bucket pushes into the number of
maximal dead spans, and it removes the input to the radix sort of §3.2 items 7–9.

**Why the end state is provably unchanged:** `coalesce_free_list` already merges
exactly this adjacency one statement later, and it still runs afterwards to merge
against spans surviving from earlier cycles.

**What genuinely changes, and must be reviewed:**
1. **Routing.** A merged run is large and routes to `free_large` rather than a
   `free_small` bucket. That is what `coalesce_free_list` would have made of it
   anyway, but it happens one statement earlier.
2. **`coalesce_threshold` backoff.** With fewer adjacent blocks arriving, the
   `merged.len() == before` arm at `arena.rs:1462` fires more often and
   quadruples the threshold `Arena::alloc`'s last-resort merge consults. This is
   the only policy side effect and it is the one to measure.
3. **The memset must stay per-object.** `write_bytes` at `zgc.rs:9333` zeroes
   `alloc_size(header)` bytes. Merging *that* would zero padding and any gap the
   run detector's exact-adjacency test tolerates. **Do not merge it.**
4. **`CRATONVM_ZGC_STARTBITS=0` (the `Hash` arm) returns bases UNORDERED.** The
   run detector must be correct there, not merely useless. It is: the test is
   exact address adjacency computed from real sizes, so an unordered stream
   simply flushes runs of length 1. **This is the sharpest hazard in P2 and a
   reviewer must check it explicitly.**

### P3 — NOT RECOMMENDED without further work
`dead: Vec<usize>` (`zgc.rs:9293`) exists only to drive
`registry.remove` at `zgc.rs:9435`. A range-clear over P2's runs would be far
cheaper — but `zgc.rs:9431`'s comment states the invariant it would break: the
registry must be pruned **in place**, never wholesale, because an allocation
registered between the mark snapshot and this publish would otherwise be erased,
leaking it and making `is_object_address` deny it while it is reachable. A range
clear cannot distinguish those. **Leave it.**

### What P1+P2 will NOT buy — and this is the point
Even with both, the cycle still makes **two full passes over every registered
object** (`zgc.rs:8978` and `9303`), still memsets every dead byte, and still
pays the O(capacity) `snapshot()`. On the measured `-Xmx1g` cycle that is
2 × 16.7 M header touches over ~800 MB plus a 778 MB memset. A generous floor is
**150–300 ms per cycle — 30–60x HotSpot's 4.9 ms, not 249x.**

**A sweep optimisation is worth perhaps 3–5x of the GC component and nothing at
all of the 4.37x allocator gap §2.2 measures at `-Xmx8g`. The structural answer
is a copying young generation, which is what `gen_heap.rs` already implements and
what §2.3 shows working. That is a backend decision (N3), not a patch.**

### How to validate, in order
1. Build; `cargo test -p cratonvm-gc`.
2. The nine vectors of §5.1, **with their real flags**, both collectors.
3. Re-run them under `CRATONVM_DBG_GC_STRESS=<small>` so each takes many cycles
   rather than one — the flag has been honoured by this backend only since
   2026-08-14 (`zgc.rs:8880`, predicate at `8891`).
4. `CRATONVM_ZGC_RELOCATE=1` (a second `bases()` consumer) and
   `CRATONVM_ZGC_STARTBITS=0` (the unordered arm), explicitly.
5. `CRATONVM_DBG_ZGC_CORPSE=1` — the pre-sweep extent survey at `zgc.rs:9288-9289` is
   the check that catches exactly the class of damage P2 could cause.
6. The §2.1 A/B, ≥12 interleaved rounds, at 8g/1g/256m, checksum on every run.

---

## 8. Why this lane changed no code

The brief allows a costed plan and asks for confidence. This lane has none to
offer for an *applied* change, for four reasons that compound:

1. **It cannot build.** The orchestrator owns the target-dir lock; `cargo build`,
   `check` and `test` are all forbidden here. A GC sweep edit that has not
   compiled is not a candidate.
2. **It therefore cannot re-run §5.1.** A change to the sweep that passes the
   vectors on the *old* binary is not evidence of anything.
3. **It therefore cannot A/B.** §2's authority is 96 interleaved runs; there is
   no second binary to interleave against.
4. **The measured win is in the wrong place anyway.** §7's closing paragraph:
   P1+P2 address ~50% of a cycle whose floor is still 30–60x HotSpot, on a
   workload where simply selecting the other backend is 6.35x *today* with no
   code change at all. Spending the risk budget of a blind GC edit to buy a
   fraction of the smaller lever would be a bad trade even if the edit compiled.

`gc/src/gen_heap.rs` and `gc/src/zgc.rs` are **byte-identical to HEAD**.

---

## 9. Reproduction

```bash
export JAVA_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
CV="C:/craton/target-fcheck/release/cratonvm.exe"

# §1 — the whole finding, in two commands
cd bench
"$CV" --java-home "$JAVA_HOME" -Xmx1g --verbose:gc                        -cp . BinTreesClassic 18
"$CV" --java-home "$JAVA_HOME" -Xmx1g --verbose:gc -XX:+UseGenerationalGC -cp . BinTreesClassic 18
# and the static half:
grep -c moving_young gc/src/zgc.rs        # 0
sed -n '769p' vm/src/config.rs            # gc_algorithm: GcAlgorithm::Zgc

# §2 — the A/B. Three arms, order rotated 3 ways by round index, wall from
# date +%s%N either side of the process, checksum parsed from every run.
#   H in {8g, 1g, 256m}; 12 rounds at 1g and 256m, 8 at 8g.

# §3 — the per-cycle table: one run per (heap, collector), grepping
#   'zgc-real:|zgc-reclaim:|generational:|decision|moving_young:'.

# §5.1 — the vectors. javac regression-suite/src/{the nine}.java into a
# scratchpad dir (only 3 of 9 are prebuilt in regression-suite/build), then per
# vector: HotSpot raw -> extract; CratonVM with class_cv_args' real flags and
# --jdk-only -> extract; compare keys, rc, and grep for VM crashes.
#   extract() { sed 's/\x1b\[[0-9;]*m//g' | grep -aE '^(PASS|CK) ' ; }
#   RPriorityQueueGc: --nojit --Xmx 64m     RTreeRangeGc: --Xmx 64m
#   RClassUnloadSweepGen: -XX:+UseGenerationalGC
```

Two probes were written; both live in this session's scratchpad, outside the
repository, and both are fully described here:

* **`GcShape2.java`** — a constant-dead-bytes / varying-dead-object-count probe
  built to separate "cost per dead object" from "cost per dead byte". **It was
  not needed**: §3.1's reading of the shipping `--verbose:gc` counters answers
  the same question directly and without a new workload, so the probe's numbers
  are not used anywhere in this record and are not reported.
* **`HmPhase.java`** — `bench/HashMapOnly.java`'s kernel verbatim, with
  `System.currentTimeMillis()` printed at `main` entry and exit so the shell can
  split process wall into startup / kernel / in-`main`-outside-kernel / shutdown.
  This is §5.3's table.

---

## 10. Verification

* `rustfmt --edition 2021 --check gc/src/zgc.rs` — exit 1, **46 pre-existing
  hunks in `zgc.rs` itself** plus 108 in the `gc/src/zgc/` submodule tree that
  the same invocation pulls in (`barrier.rs` 18, `relocate.rs` 20, `census.rs`
  12, `remembered.rs` 12, `mark.rs` 9, `metrics.rs` 8, `forwarding.rs` 7,
  `page.rs` 7, `vaddr.rs` 5, `generation.rs` 4, `adapters.rs` 3, `tlab.rs` 3).
* `rustfmt --edition 2021 --check gc/src/gen_heap.rs` — exit 1, **58
  pre-existing hunks.**
* **Every one of those is pre-existing and unattributable to this lane, because
  this lane wrote no byte of either file.** Both were run in place, in this tree.
* `tr -cd '\r' < <file> | wc -c` — **0** for this record.
* No `cargo build`, `cargo check` or `cargo test` was run. No state-changing git
  command was run, including `git stash`. `regression-suite/run.sh` was not run.

---

## 11. What this lane did NOT do

* **Did not change any Rust file.** §8. §7 is a plan, not a diff, and its
  performance estimates are **PREDICTED** — the only measured claim about P1/P2
  is the code shape they address.
* **Did not build or compile-check anything Rust**, so §7's plan has not been
  through a type checker.
* **Did not measure CPU time.** Wall only, on a 32-CPU box with several agents
  on it. §2.2's spreads are what that distinction looks like.
* **Did not use a native profiler.** §3.2's attribution of the 73 ns is read off
  the *source*, matched against the *measured* per-cycle law of §3.1. It is not a
  sampled profile, and the split between its ten items is **NOT MEASURED** — only
  the total is.
* **Did not explain §5.3.** The 3.5–5.5 s the generational backend spends inside
  `main` after the kernel returns on `HashMapOnly 8000000` is measured and
  unattributed. `--dump-phase-report` was tried on both arms and returned a
  `basis_wall_ns` of ~0.5 ms with every phase bucket zero, i.e. it did not
  instrument the run; that instrument was abandoned rather than trusted.
* **Did not run the Tomcat, Spring Boot, H2, Keycloak or Elasticsearch suites**,
  which are the workloads N3's decision actually turns on. Every throughput
  number here is from a single-collection allocation kernel — the same hole
  `G20-1` §11 declares.
* **Did not re-run `G20-1`'s startup, JIT-split, native-boundary or field-read
  series**, and does not contradict any of them. Only its §4 is corrected.
* **Did not write to `regression-suite/build`**; §5.1's vectors were compiled to
  the scratchpad so the repository tree is unchanged.
* **Did not edit `INDEX.md` or `README.md`.**
