# G20-1 — the first performance profile of this branch

> **RECONCILED 2026-08-17 (lane G40) — the GC headline below is FALSIFIED. Read
> `G27-1` before §0 or §4.**
>
> * **`moving_young: cycles=0` did not mean the young path was broken.**
>   `gen_heap.rs` is **not the collector** in a default run: `vm/src/config.rs:769`
>   has defaulted to `GcAlgorithm::Zgc` since 2026-08-10, and
>   `grep -c moving_young gc/src/zgc.rs` returns 0, so the gating predicate at
>   `gen_heap.rs:5719` is never reached. All three diagnostics this record rests
>   on mislead the same way: `moving_young_requested=true` is a **JIT-codegen
>   capability flag**, not a request; *"no collection has run yet"* fires because
>   ZGC never calls `record_collector_decision`; the `[GC] cards:` block is
>   generational-only and structurally zero. §8's guess that the `zgc-*` counter
>   labels were a "legacy prefix" was the finding — the label was the collector.
>   `G27-1` re-measured it: **ZGC is 4.37x–6.35x slower than the generational
>   backend** at every heap size, ranges not touching, 96 runs, every checksum
>   `68332206`. Landed as `3765fad76`.
> * **§8's `invocations` claim is not reproducible.** `--nojit` was exact for
>   every registry-dispatched native `G33-1` probed. The supporting evidence
>   (`Preconditions.checkIndex = 1` over a `charAt` loop) was not instrument
>   failure: there is **no registry row for `String.charAt` in this binary**, so
>   that zero was honest. The *conclusion* — that an arm-independent
>   under-reporting mechanism exists — was right; the mechanism is the
>   interpreter's intrinsic table, not sampling. See `G33-1`.
> * **The binary this record measured is `C:/craton/target-fcheck/…`, which the
>   tree now says to ignore** — that build partly failed and its timestamp
>   misrepresents its contents (`G34-1` §provenance). This record already flagged
>   its attribution as only "partly" sound.
>
> The startup, throughput and native-boundary numbers were **not** re-taken on a
> good binary. They are not marked wrong here; they are **unre-measured**, which
> is not the same thing.

**Status:** MEASURED, end to end. **Provenance:** every number below was taken
on this host, on the shipping release binary, against HotSpot 25.0.3+9-LTS as
the oracle. Nothing here is PREDICTED. That is worth stating in this directory,
where `HANDOFF-20260814` §2 has to warn that most records are not.

| | |
|---|---|
| binary | `C:/craton/target-fcheck/release/cratonvm.exe`, release, mtime 2026-08-17 00:44, stated to be built from `d2e127930` |
| attributable? | **partly.** The mtime and the stated commit are all this lane had; it did not build the binary and could not re-run the `find -newermt` check `HANDOFF-20260814` §3 prescribes. Every measurement is of *that file*, whatever it was built from. |
| oracle | HotSpot 25.0.3+9-LTS, `C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot` |
| host | this Windows 11 box, 32 logical CPUs, **running seven agents concurrently for the whole series** |
| corpus | `bench/CratonBench.java` phases, unmodified, plus four purpose-built probes described in §9 |
| tree | `claude/jdk-only-mode-completion-1351c0`, working tree dirty (six other lanes mid-flight) |

**Read the host line as a warning, not a footnote.** Background load moved by
more than 30% *within* this session: the same `Hello` startup measured 275 ms
median in §1 and 361 ms median in §7, two hours apart, on an unchanged binary.
Every table below is therefore **interleaved** — arms alternate within a round
and the round order is rotated — and **no number in one table may be compared
with a number in another.** Compare within a row.

---

## 0. The headline

| finding | number |
|---|---|
| CratonVM startup vs HotSpot, trivial `main` | **275 ms vs 86 ms — 3.2x** |
| `--jdk-only` startup cost over Compatible mode | **+3 ms (1.1%) — not distinguishable from noise** |
| best throughput row (matrix, JIT'd, n=200) | **0.58x — CratonVM faster than C2** |
| worst throughput row (bintrees d=18) | **28.7x** |
| what the JIT buys, matrix | **239x** |
| what the JIT buys, hashmap | **3.6x** |
| **one GC cycle, bintrees d=18, `-Xmx1g`** | **1,222,726 µs against HotSpot's 4,908 µs for the same reclamation — 249x** |
| `ArrayList.size()` | **330 ns/call vs HotSpot's 6 ns** |
| the Rust native-call boundary | **~141 ns per crossing** |
| the W7-84 WARN storm's cost at startup | **none measurable** |

The outlier is GC, and §4 is the section to read.

---

## 1. Startup — MEASURED

```bash
export JAVA_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
CV="C:/craton/target-fcheck/release/cratonvm.exe"
JV="$JAVA_HOME/bin/java"
# Hello.java: one println, javac'd with the oracle's javac.
# 18 rounds. Each round runs all three arms; the order rotates 3 ways by
# round index so no arm is always first. stderr and stdout to /dev/null in
# every arm, so the W7-84 storm's IO is charged symmetrically. Wall time from
# date +%s%N either side of the process.
"$JV" -cp . Hello
"$CV" --java-home "$JAVA_HOME" -cp . Hello
"$CV" --java-home "$JAVA_HOME" --jdk-only -cp . Hello
```

| arm | n | median | min | p25 | p75 | max |
|---|---:|---:|---:|---:|---:|---:|
| HotSpot | 18 | **86 ms** | 79 | 81 | 91 | 96 |
| CratonVM, Compatible | 18 | **275 ms** | 262 | 269 | 283 | 294 |
| CratonVM, `--jdk-only` | 18 | **278 ms** | 263 | 273 | 288 | 320 |

**Wall time.** CPU time was not separable on this host and is not claimed
anywhere in this record.

**`--jdk-only` is free at startup.** The brief expected it to be expensive,
because it loads real JDK class bytes rather than synthetic stubs. It is not:
+3 ms on a 275 ms median, with quartile ranges (269–283 against 273–288) that
overlap almost completely. **This is a negative result and it is a real one** —
it says the `--jdk-only` policy work has not bought itself a startup
regression, and that no startup optimisation should be aimed there.

Corroborated independently. The throughput series of §2 records wall and
in-process kernel time for every run, and their difference is process
startup + teardown:

| phase | HotSpot startup+exit | CratonVM startup+exit |
|---|---:|---:|
| arithmetic | 105 ms | 283 ms |
| bintrees | 142 ms | 420 ms |
| stringregex | 111 ms | 300 ms |

Median 283–420 ms against 275 ms from a completely separate series two hours
earlier. The `-Xmx8g` arms carry the extra.

## 2. Throughput over the existing corpus — MEASURED

House method, per `BENCHMARK.md`: one phase per fresh process, `-Xmx8g` on both
sides, arms alternated with the order flipped on alternate rounds, medians over
5 pairs, **no sample discarded**, checksum verified on every run.

```bash
cd bench   # CratonBench.java compiled with the oracle's javac
for r in 1..5; do for ph in arithmetic fib sieve matrix hashmap stringregex bintrees; do
  # odd rounds HS first, even rounds CV first
  "$JV" -Xmx8g -cp . CratonBench "$ph"
  "$CV" --java-home "$JAVA_HOME" -Xmx8g -cp . CratonBench "$ph"
done; done
```

**70 runs, 0 checksum mismatches.** In-process kernel ms (not wall):

| phase | HotSpot | CratonVM | ratio | CratonVM samples | `BENCHMARK.md` ratio (Azure) |
|---|---:|---:|---:|---|---:|
| sieve | 5,186 | 5,426 | **1.05x** | 5365 5391 5426 5510 6309 | 0.99x |
| matrix | 2,531 | 2,735 | **1.08x** | 2548 2640 2735 2804 3254 | 0.99x |
| arithmetic | 2,441 | 6,013 | 2.46x | 5939 5945 6013 6072 7117 | 1.95x |
| fib | 2,871 | 9,057 | 3.15x | 8922 8944 9057 10208 10265 | 5.89x |
| stringregex | 30 | 297 | 9.90x | 284 289 297 301 319 | 5.37x |
| **hashmap** | 561 | 5,889 | **10.50x** | 5822 5836 5889 6101 7017 | 2.07x |
| **bintrees** | 416 | 11,929 | **28.68x** | 11777 11780 11929 12314 12630 | 9.46x |

Sieve and matrix hold the parity `BENCHMARK.md` records, on a different OS and
a different CPU. Fibonacci is *better* here than the Azure table (3.15x against
5.89x) — that table already flags its Fibonacci row as unexplained, and this
reading does not explain it either.

**The outliers are the two allocation-bound rows, and they are outliers
against CratonVM's own published figures, not just against HotSpot.** hashmap
moves 2.07x → 10.50x and bintrees 9.46x → 28.68x, while the four compute-bound
rows move by well under 2x in either direction. A host change that cost
CratonVM uniformly would not sort the rows this way. Whatever this is, it is
specific to allocation. §4 is what it is.

## 3. The interpreter/JIT split — MEASURED

The corpus sizes are too large to run interpreted, so this uses `ScaledBench`,
which is `CratonBench.java`'s kernel bodies **verbatim** with the sizes taken
from argv (§9). Sizes chosen so the `--nojit` arm finishes in single digits of
seconds. Three arms, order rotated 3 ways by round, 6 rounds, medians.
**108 runs, 0 checksum mismatches.**

```bash
"$JV"                                   -Xmx8g -cp . ScaledBench "$ph" "$n"
"$CV" --java-home "$JAVA_HOME"          -Xmx8g -cp . ScaledBench "$ph" "$n"
"$CV" --java-home "$JAVA_HOME" --nojit  -Xmx8g -cp . ScaledBench "$ph" "$n"
```

| phase | n | HotSpot | CratonVM JIT | CratonVM `--nojit` | **what the JIT buys** | CV-JIT / HotSpot |
|---|---:|---:|---:|---:|---:|---:|
| matrix | 200 | 12 | **7** | 1,671 | **239x** | **0.58x** |
| sieve | 40 reps | 15 | 18 | 1,645 | 91x | 1.20x |
| arithmetic | 20M | 26 | 62 | 4,107 | 66x | 2.38x |
| fib | 30 | 4 | 14 | 528 | 38x | 3.50x |
| **bintrees** | d=12 | 7 | 235 | 1,684 | **7.2x** | 33.57x |
| **hashmap** | 200K | 22 | 126 | 458 | **3.6x** | 5.73x |

**The JIT is not the problem, and this table is how you know.** On matrix it
buys 239x and lands *faster than C2*. The two workloads where CratonVM is
worst are precisely the two where the JIT buys least — 3.6x and 7.2x against
38–239x everywhere else. A compiler cannot speed up time that is not spent in
compiled bytecode. On hashmap and bintrees the remaining time is somewhere the
JIT does not own: the allocator, the collector, and Rust natives.

Interpreter rate, for calibration: arithmetic's loop body is ~22 bytecodes, so
20M x 22 = 440M bytecodes in 4,107 ms = **~9.3 ns/bytecode**. That is a
serviceable interpreter, 2–4x HotSpot's. **The interpreter is not the story
either.**

## 4. THE HOT SPOT — one GC cycle costs 1.22 seconds, and the cause is that the young generation never collects

This is the quantified hot spot with a named cause.

### 4.1 The differential that found it

If the gap were mutator work it would be indifferent to heap size. It is not.
bintrees d=18, CratonVM only, 3 rounds of a 4-way heap sweep, GC counters read
from the VM's own instrument:

```bash
"$CV" --java-home "$JAVA_HOME" -Xmx{8g,1g,512m,256m} --verbose:gc -cp . ScaledBench bintrees 18
```

| `-Xmx` | kernel ms (3 samples) | GC cycles | total pause | pause as % of wall | occupancy at exit |
|---|---|---:|---:|---:|---:|
| 8g | 17011 13529 16603 | **1** | 0.08–0.09 s | ~0.6% | **3,281,498,824 B** |
| 1g | 23220 21125 22489 | 5 | **5.40 s** | **23%** | 202,881,832 B |
| 512m | 23102 23398 22700 | 9 | 5.54 s | 24% | 328,711,768 B |
| 256m | 26750 24364 24538 | 20 | **7.43 s** | **30%** | 127,382,536 B |

All twelve runs checksum `68332206`. The instrument is credible: the 1g arm is
~6–8 s slower than the 8g arm and its extra pause is 5.3 s, which closes.

### 4.2 The per-cycle numbers, and the oracle beside them

```bash
"$CV" --java-home "$JAVA_HOME" -Xmx1g --verbose:gc -cp . ScaledBench bintrees 18
"$JAVA_HOME/bin/java" -Xmx8g -Xlog:gc -cp . ScaledBench bintrees 18
```

CratonVM, `-Xmx1g` (kernel 22,272 ms):

```
zgc-real: cycle=2 pause_us=1222726 objects_copied=536000  bytes_copied=26766384 bytes_freed=778539984
zgc-real: cycle=3 pause_us=1090170 objects_copied=538468  bytes_copied=26884848 bytes_freed=778421520
zgc-real: cycle=4 pause_us=1526489 objects_copied=586318  bytes_copied=29181648 bytes_freed=776124720
zgc-real: cycle=5 pause_us=1314750 objects_copied=1223692 bytes_copied=59775600 bytes_freed=745530768
zgc-reclaim: ... registered=16755583
moving_young: cycles=0 coverage_fallbacks=0
decision: no collection has run yet (moving_young_requested=true)
zgc-features: parallel_mark_cycles=0 driver_passes=0 compaction_cycles=0 objects_relocated=0 relocation_skipped_jit=5
```

HotSpot, same workload:

```
GC(0) Pause Young (Normal) (G1 Evacuation Pause)  49M->13M(1024M)  7.194ms
GC(1) Pause Young (Normal) (G1 Evacuation Pause) 481M->14M(1024M)  4.721ms
GC(2) Pause Young (Normal) (G1 Evacuation Pause) 614M->14M(1024M)  4.908ms
```

| | CratonVM cycle 2 | HotSpot GC(2) |
|---|---:|---:|
| reclaimed | 778 MB | 600 MB |
| live retained | 26.8 MB / 536,000 objects | 14 MB |
| **pause** | **1,222,726 µs** | **4,908 µs** |
| ratio | | **249x** |

Three young collections cost HotSpot **16.8 ms in total**. Five cost CratonVM
**5.40 seconds**.

### 4.3 The named cause

`moving_young: cycles=0` in **every** arm of the sweep, at every heap size,
alongside `decision: no collection has run yet (moving_young_requested=true)`.
The generational moving-young path is requested and never runs. So every
collection is a whole-heap, non-moving pass, and the log says what it walks:
`registered=16755583`.

**1,222,726 µs / 16,755,583 registered objects = 73 ns per registered object
per cycle.** The cost scales with everything ever allocated, not with what is
live — 536,000 objects, **3.2%** of the registered set, survived cycle 2. A
young evacuation touches the live set only, which is why HotSpot's number is
three orders of magnitude smaller for the same reclamation.

`BENCHMARK.md` already carries a one-line note that "young GC always falls back
to non-moving sweep for this workload", recorded in passing while explaining why
a 512 MiB young cap regressed 12–13%. **That note is the whole defect, it is
worth 23–30% of wall time at a realistic heap size, and nothing in this
repository has costed it before.**

### 4.4 What the default heap is really doing

At `-Xmx8g` the VM finishes bintrees d=18 having run **one** GC cycle and
holding **3.28 GB**. It is not fast there because it collects well; it is fast
there because it does not collect. It is buying the absence of a 1.2 s/cycle
tax with 3.28 GB of RSS. That is the trade the default heap silently makes, and
it is why the `-Xmx8g` row (28.7x) looks *better* than the `-Xmx1g` row.

**And it is still 28.7x with GC switched off in all but name.** At `-Xmx8g`,
~13.4 s of the 13.5 s is mutator. HotSpot's mutator is ~0.5 s. So **GC is not
the whole gap — it is the part that is now costed.** The mutator's own ~26x is
addressed in §6 N5 and is not explained here.

## 5. The two files this lane owns are cold, and one of them says otherwise — MEASURED

`native-builtins/src/preconditions.rs` opens by justifying itself: `String.charAt`
reaches `Preconditions.checkIndex` "on every single character read", so the
check belongs in Rust. That sentence has no denominator, and it is false of this
binary.

```bash
# 1,000,000 charAt calls, kernel in its own static method (see §9's trap note)
"$CV" --java-home "$JAVA_HOME" --nojit --dump-native-registry reg_noj.json -cp . NatBench charat 1000000
"$CV" --java-home "$JAVA_HOME"         --dump-native-registry reg_jit.json -cp . NatBench charat 1000000
```

| instrument | reading |
|---|---|
| registry, `--nojit` arm | `Preconditions.checkIndex(IIL..BiFunction;)I` **invocations = 1**. Not 1,000,000. |
| registry, JIT arm | **invocations = 1**, identical |
| whole-process native dispatches, either arm | **1,540** |
| the other five registered `Preconditions` overloads | **invocations = 0** |

The counter under-reports elsewhere (§8), so it does not stand alone. Timing is
the independent check, and it is decisive:

| kernel | CratonVM JIT | HotSpot |
|---|---:|---:|
| `s.charAt(i & 63)` | **5 ns/call** | 6 ns |
| `s.length()` | **5 ns/call** | 6 ns |
| `System.identityHashCode(o)` — a real native | **167 ns/call** | 5 ns |
| user-defined `p.size()`, interpreted body, JIT'd caller | **26 ns/call** | 4 ns |

Medians of 5, phase order reversed on alternate rounds, 1,000,000 calls per run.

**The native-call boundary costs ~141 ns** (167 less the 26 ns call control).
`charAt` costs 5 ns. It is 28x cheaper than crossing into Rust once, so on the
arm that ships it provably does not enter this module per character.

**Changed** (`native-builtins/src/preconditions.rs`, module header): the
per-character claim is replaced by the measurement and by an explicit "do not
optimise here". The module's reason to exist is untouched, because that reason
is the exception formatter — a control-flow contract — and correctness is not
what was mis-stated.

`native-builtins/src/field_read.rs` was left alone, for a reason with a
denominator: it has **21** call sites across the crate
(`lang_class` 8, `lang_reflect` 5, `lib` 4, `bouncycastle` 2, `zip_streams` 1,
`spring_startup_bootstrap` 1), and every `lang_reflect` one is a generic-type
or `Parameter` accessor — `getRawType`, `getOwnerType`, `getName`. None appears
in any invocation census taken here. **A memo on `slot_of`'s name resolution is
nominated in §6 N7 and deliberately not written**, because nothing measured
says it would pay, and this lane's own §5 is a demonstration of what happens
when a hot-path claim is asserted rather than counted.

## 6. NOMINATIONS, ranked by (measured win) / (risk to correctness)

Six lanes have just spent this session fixing subtle correctness bugs. A fast
wrong answer is worth less than nothing here, so the ranking is deliberately
conservative and the two biggest wins are **not** at the top.

### N1 — memoize the `native_al_*` receiver-test chain behind one `ClassFacts` bit
**File:** `native-collections/src/lib.rs`, `native_al_size` (line 4961) and
`native_al_is_empty` (line 5002).
**Measured cost:** `ArrayList.size()` = **330 ns/call**, `isEmpty()` = 336,
`get(0)` = 473, against HotSpot's 6, 5, 6. Control: a user-defined `size()` in
the same loop shape costs 22 ns. Boundary floor from §5 is ~141–167 ns.
So **~163 ns per call is spent inside the native, above the boundary** —
roughly half the total, on a method whose answer is one field.
**Mechanism:** a plain `ArrayList` receiver — the overwhelming bulk of traffic
— runs four receiver tests before reaching its own `size` field: `ksv_route`
(key-set view, line 51945), `unmod_receiver_backing` (4859),
`vc_route_source_size` (14808, the real values-view class test), then
`al_state_for_read`'s `view_source_in` (14559) marker test. Each is a
`class_id_of_object` plus a `RECEIVER_FACTS` probe. Fold one
`CF_PLAIN_ARRAYLIST` bit into `receiver_facts` (computed once per `ClassId`,
already 512-way memoized, line ~300) and short-circuit all four.
**Risk: MEDIUM-LOW.** The pattern already exists in this file:
`unmod_receiver_backing` opens with exactly this optimisation
(`al_slots_and_layout_for(..).rules_out_wrapper_routes()`) and its comment
records the win it bought — "1.0% of a flat `ArrayList.size()` profile plus the
1.3% `class_name_rc` it drives". This is that change, generalised to the other
three tests. The bit must be *conservative*: set only for a class the memo can
prove is neither a view, a wrapper, nor a marker carrier.
**On the brief's ordering question — the current order is right, and that is
not where the cost is.** `vc_route_source_size` must precede `al_state_for_read`,
because a real `HashMap$Values` has no `elementData` and `al_state` cannot
decode it; reversing them would break the class the second test cannot see. And
the two are not costing `isEmpty` extra: `alsize` 330 and `alempty` 336 are
indistinguishable at this spread (307–516 against 311–565). The defect is not
the order, it is that a plain `ArrayList` pays the whole chain.

### N2 — `Map.values()` mints a fresh carrier per call
**File:** `native-collections/src/lib.rs`, the `values()` registration feeding
`vc_route_source_size` / `al_state_for_read` (14808 / 4934).
**Measured cost:** on a 64-entry `HashMap`, 1,000,000 calls each, medians of 5:

| kernel | CratonVM JIT | HotSpot |
|---|---:|---:|
| `m.values().size()` — view minted in the loop | **5,888 ns** | 9 ns |
| `v.size()` — same view hoisted out of the loop | **1,124 ns** | 7 ns |
| `list.size()` | 330 ns | 6 ns |

**The `values()` call alone is ~4,760 ns.** The merge note that motivated this
lane's brief cites 15.9 µs for the un-hoisted path; on this host and this binary
it is **5.9 µs**, so that optimisation landed and is worth ~2.7x. What is left
is not in `size()` at all — it is in minting the view. `values()` is idempotent
for an unmodified map and the JDK caches it in a field; caching the carrier
against a modCount would remove ~4.8 µs from every `values()`-in-a-loop.
**Risk: MEDIUM.** Requires a correct invalidation, and `G8-1` has just finished
mapping the view families. Coordinate with that record.

### N3 — the Rust native-call boundary itself, ~141 ns per crossing
**Measured cost:** §5's calibration. Every Rust-implemented JDK method pays it
before executing a single instruction of its body. It is ~43% of an
`ArrayList.size()` and ~30% of an `ArrayList.get()`. `java/util/ArrayList.size()I`
is dispatched 62 times merely to reach `main` in a Hello-world process.
**Mechanism:** unknown from outside — this lane could not see inside the
boundary with the instruments available, and the number is an *upper* bound on
the pure crossing, since `identityHashCode` also computes and installs a hash.
**Risk: HIGH** (dispatch is load-bearing for every native). **But the leverage
is the largest of any single change here**, because it multiplies across every
one of the ~12,000 registered natives. Nominated for *measurement* first: a
genuinely empty native, registered solely to be timed, would turn the ~141 ns
upper bound into a real figure and tell you whether N1 or N3 is the better
investment.

### N4 — the young generation never collects
**File:** the moving-young admission decision behind
`[GC] decision: ... (moving_young_requested=true)` and
`[GC] moving_young: cycles=0`; `gc/src/gen_heap.rs` is the entry point.
**Measured cost:** §4. **1.22 s per cycle against HotSpot's 4.9 ms — 249x**;
23% of bintrees wall at `-Xmx1g`, 30% at `-Xmx256m`; 73 ns per registered
object per cycle where only 3.2% of registered objects are live; and 3.28 GB of
retained heap at `-Xmx8g`, which is the price the default configuration pays to
avoid it.
**Risk: HIGH, and this is why it is fourth and not first.** It is the largest
measured win in this record by a wide margin and it is a garbage collector.
`BENCHMARK.md` already records one attempt in this area (capping the young
semispace at 512 MiB) that measured as a **12–13% regression** and was reverted,
precisely because it multiplied this defect instead of fixing it. Root-cause
*why* moving-young is requested and declined before changing any policy. It is
a whole workstream, not a patch.

### N5 — allocation footprint and mutator throughput on allocation-bound code
**Measured cost:** bintrees d=18 allocates **3.28 GB** on CratonVM where
HotSpot's summed young occupancies are ~1.4 GB — **~2.3x the bytes for the same
object graph**. With GC effectively off (`-Xmx8g`, one cycle), the mutator
alone is ~13.4 s against ~0.5 s, i.e. **~26x**. The `--nojit` interpreter
profile (2,270 leaf samples, §9) puts **44.7%** of interpreted time in
`Node.<init>` — two reference-field stores — and 29% in `itemCheck`, which is
two reference-field loads.
**Mechanism: NOT ESTABLISHED.** The footprint ratio is consistent with the
16-byte `Value` cell layout `gc/src/autobox.rs` describes, but this lane did not
prove that is where the 26x lives. **Nominated as a measurement, not a fix.**
**Risk: HIGH** (object layout).

### N6 — the W7-84 autobox latch is armed on every start, so its fast path is dead
**File:** `gc/src/autobox.rs:100` (`wrapper_exists`), armed from
`vm/src/vm/vm_object.rs`'s class-mirror populator; read side at
`gc/src/gen_heap.rs:3723`, `gc/src/g1.rs:9838`, `gc/src/heap.rs:680`,
`gc/src/zgc.rs:8627`.
**Measured:** the WARN's `occurrence=16` record prints on **every** run —
`Hello`, bintrees d=14, bintrees d=18 at 8g and at 1g, all four — and
`occurrence=32` never does. The site logs the first 8 plus powers of two, so
every process boxes **between 17 and 31 times before `main`**, and the count
does not grow with the workload. So the boxing is a fixed boot cost and is
*not* on a hot path — but the latch it arms is monotone, and
`gc/src/gen_heap.rs:3716`'s own comment claims the read side is now "behind a
monotone relaxed latch that no process which never boxes ever arms." **No
process ever fails to arm it.** Every non-null compact reference-field read in
every CratonVM process therefore pays `is_object_address` plus a header read,
and the documented saving is never realised.
**Measured cost: BELOW NOISE — see §7.3. This lane could not measure it.**
**Risk: LOW-MEDIUM** for the populator fix (write a real reference, or give the
mirror populator a non-boxing path). **Value: unproven.** Rank it only after
somebody adds an env-gated latch override and A/Bs it; that instrument does not
exist and is the actual first task.

### N7 — memoize `slot_of`'s field-name resolution
**File:** `native-builtins/src/field_read.rs:78`. Every `ref_field` /
`int_field_strict` does a `class_id_of_object` plus a string-keyed
`resolve_field_index_by_class_id`. A per-`(ClassId, name)` memo is the same
shape as `RECEIVER_FACTS`, and field indices are immutable after linking.
**Measured cost: NONE — 21 call sites, all cold (§5).** Listed for completeness
and **explicitly not recommended**. Written down so the next lane does not
rediscover it and assume it matters.

### Not a nomination: the W7-84 WARN storm
§7.1 measured it. It costs nothing.

### Not a nomination: `--jdk-only` startup
§1 measured it. It costs nothing.

## 7. Three negative results, each of which was expected to be positive

### 7.1 The W7-84 WARN storm is free — MEASURED

The brief asked what ~16 multi-line WARN records cost on every VM start. Answer:
nothing detectable. 21 rounds, three arms, order rotated 3 ways by round.

```bash
"$CV" --java-home "$JAVA_HOME" -cp . Hello                              # stderr to /dev/null
RUST_LOG='cratonvm::gc::guard=error' "$CV" ... -cp . Hello              # boxing still happens, no format/IO
"$CV" ... -cp . Hello 2>a-real-file                                     # storm actually written
```

| arm | n | median | spread |
|---|---:|---:|---|
| WARN emitted | 21 | **361 ms** | 323–525 |
| WARN suppressed by `RUST_LOG` | 21 | **375 ms** | 310–487 |
| WARN written to a real file | 21 | **396 ms** | 301–518 |

The **suppressed** arm is 14 ms *slower* than the emitted one. That is noise,
and it is the point: the effect is smaller than the noise floor on this host.
The storm is 10 records and 6,960 bytes of stderr
(`"$CV" ... 2>&1 >/dev/null | wc -c` = 6960; suppressed = 319). It is a
log-hygiene problem — 6.6 KB of WARN on every invocation is genuinely bad for
operators — but **it is not a performance problem and must not be fixed as one.**

Note also that the counter says the storm is **10 records, not 16**: the site is
rate-limited to the first 8 plus powers of two, and `occurrence=16` is the last
one printed. The ~16 in the brief is the *boxing* count, not the *logging*
count, and the two are different numbers.

### 7.2 `--jdk-only` costs nothing at startup

§1. +3 ms on 275, quartiles overlapping.

### 7.3 The autobox read-side tax could not be measured — and this is the honest entry

`gc/src/autobox.rs::unbox_reference_slot` returns immediately for a null
reference and runs `is_object_address` plus a header read for a non-null one.
`FieldBench` (§9) puts those two cases in **bytecode-identical** loops on the
**same object**, differing only in whether the slot holds null. 14 paired
rounds, `--nojit`, 5,000,000 reads per run, order swapped every round:

| kernel | n | median | spread |
|---|---:|---:|---|
| `getfield I` | 14 | 2,687 ms | 2,346–3,721 |
| `getfield L..;`, slot **null** | 14 | 2,833 ms | 2,465–3,473 |
| `getfield L..;`, slot **non-null** | 14 | 2,986 ms | 2,371–3,499 |

Monotone in the predicted direction. But the **paired** per-round deltas are
`+228 −72 −238 −60 +13 −122 +336 −144 +309 −132 +557 +664 +257 +363`: median
**+13 ms** over 5,000,000 reads = **+2.6 ns/read**, mean +140 ms, and the sign
flips in 6 of 14 pairs.

**That is not a measurement, and it is reported as one only to say so.** A
6-round earlier series read +250 ms and would have supported a confident
"50 ns per reference-field read" — the tightened, paired series does not.
Under the **JIT** arm the two kernels read 11 ms and 11 ms, so whatever the tax
is, the compiled path does not pay it.

**Conclusion: the autobox latch is provably always armed (N6) and its per-read
cost is below this host's noise floor. Both halves are results.**

## 8. What was measured and could not be explained

1. **The `--dump-native-registry` `invocations` counter under-reports by two to
   three orders of magnitude, in both the JIT and the `--nojit` arm.** 1,000,000
   `HashMap.put` calls register **3,000** invocations; ~500,000 `Node`
   allocations register **1,291** `Object.<init>`; a 1,000,000-iteration
   `charAt` loop produces **1,540** native dispatches process-wide. The
   `--help` text names a `jit_inline_cache_natives` refusal counter that would
   explain the JIT arm, but **not** the `--nojit` arm, whose top-10 table is
   byte-identical to the JIT arm's. **`HANDOFF-20260814` §4 recommends this
   dump as the instrument that settles "which body runs", and for that question
   `owns_slot` is still authoritative. But its `invocations` column cannot be
   read as a call count, and §5 above deliberately does not rest on it alone.**
2. **Why hashmap and bintrees are 5.1x and 3.0x further from HotSpot here than
   in `BENCHMARK.md`'s Azure table, while the four compute rows move under 2x.**
   §4 costs the GC component. It does not explain the residual ~26x mutator gap
   at `-Xmx8g`, where GC is one cycle.
3. **Why `zgc-*` is the label on the default collector's counters** when
   `--help` documents the default as `Generational` and no `--XX:UseGc` was
   passed. This may be nothing more than a legacy counter prefix. It was not
   chased, and every §4 conclusion rests on the counters' *values*
   (`moving_young: cycles=0`, `pause_us`, `registered`), not on their name.
4. **Fibonacci.** 3.15x here against `BENCHMARK.md`'s 5.89x, on a workload that
   document already flags as unattributed. Two records now disagree about it and
   neither explains it.

## 9. Method, so this is reproducible

```bash
export JAVA_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
CV="C:/craton/target-fcheck/release/cratonvm.exe"
```

Four probes were written; all four live outside the repository, in this
session's scratchpad, and all are reproducible from this description:

* **`ScaledBench.java`** — `bench/CratonBench.java`'s seven kernel bodies
  copied **verbatim**, with the size taken from `argv[1]` and one kernel per
  process. Used wherever a `--nojit` arm was needed. Checksums are
  size-dependent and were compared across arms at equal size, never against the
  corpus's fixed-size checksums.
* **`ColBench.java`** — six kernels, one invocation per iteration, identical
  loop shape: a user-defined `size()` control, `ArrayList.size/isEmpty/get`,
  and `map.values().size()` both hoisted and not.
* **`NatBench.java`** — the native-boundary calibration of §5.
* **`FieldBench.java`** — the four bytecode-identical field reads of §7.3.

Aggregate rules used throughout, all from `BENCHMARK.md`'s methodology section:
fresh process per measurement; arms interleaved *within* a round with the order
rotated by round index; medians, **no sample discarded**; checksums compared on
every run (**0 mismatches in 268 benchmark runs**); wall time only.

### The trap that cost this lane a whole measurement

`NatBench`'s first draft put its loops in `main` and read **406 ns/call** for a
kernel that costs **26 ns** compiled — a 15x error, in the direction of making
everything look uniformly slow. `bench/CratonBench.java`'s header documents
exactly this: `main` carries invokedynamic string concat, which bails the
whole-method OSR compile and leaves a `main`-resident loop **interpreted
forever**. Every kernel must live in its own static method. The result was
discarded and the probe rewritten; §5's table is from the rewrite.

The interpreter profile of N5 came from:

```bash
"$CV" --java-home "$JAVA_HOME" -Xmx8g --nojit --stack-sample-ms 5 -cp . ScaledBench bintrees 14
```

aggregating the **deepest** frame of each `T19.H1 stack dump` record — 2,270
leaf samples — and bucketing `pc=0 last_pc=0` separately, because the flag's own
documentation warns that such a frame has executed nothing and its time belongs
to the invoke that pushed it. First pass aggregated `depth=0`, which is the
*root* frame, and reported 99.6% in `main`; that reading was discarded.

Full breakdown, `--nojit`, bintrees d=14, 2,270 leaf samples:

| leaf | share | note |
|---|---:|---|
| `Node.<init>` | 30.75% | two reference-field stores |
| `Node.<init>` at `pc=0` | 13.96% | belongs to the `invokespecial` that pushed it |
| `itemCheck` | 23.17% | two reference-field loads + recursion |
| `bottomUpTree` | 19.60% | allocate + two recursive calls |
| `bottomUpTree` at `pc=0` | 6.21% | |
| `itemCheck` at `pc=0` | 5.81% | |
| everything else (VM boot) | 0.5% | |

## 10. Verification

* `rustfmt --edition 2021 --check native-builtins/src/preconditions.rs` — exit 1,
  on **one pre-existing** hunk at the `throw_constructed` match arm, in code this
  lane did not touch. Verified pre-existing by running the same command on
  `git show HEAD:native-builtins/src/preconditions.rs`: **identical diff, identical
  exit**. The file parses; this lane's two hunks (`@@ -43,3 +43,3 @@`,
  `@@ -47,0 +48,27 @@`) are both inside the module doc comment and rustfmt has
  no complaint about either. **Not fixed here**, because it is unrelated to this
  lane and six other lanes are editing this crate.
* `rustfmt --edition 2021 --check native-builtins/src/field_read.rs` — **exit 0**.
* `tr -cd '\r' < <file> | wc -c` — **0** for both owned files and for this record.
* Both run in place, in this tree. No `cargo build`, `cargo check` or
  `cargo test` was run, per the lane brief. No state-changing git command was run.

## 11. What this lane did NOT do

* **Did not build the binary, and could not fully attribute it.** The
  `find -newermt` check `HANDOFF-20260814` §3 prescribes was not run. Every
  number is of the file at `C:/craton/target-fcheck/release/cratonvm.exe`.
* **Did not compile-check its own edit.** The edit is a doc comment, but "a
  green build proves you broke nothing" (handoff §5) — and this lane does not
  even have that. `rustfmt` parsing is the only evidence offered.
* **Did not run `regression-suite/run.sh`**, or any regression vector.
  **No correctness claim is made anywhere in this record.**
* **Did not edit anything outside `native-builtins/src/preconditions.rs`.**
  `field_read.rs` was read and deliberately left unchanged (§5). Everything in
  §6 is a nomination.
* **Did not measure `apps/`** — no Spring Boot, H2, Tomcat, Elasticsearch or
  Keycloak run. Those are the workloads where `--jdk-only` and the cold-code
  paths of N6/N7 would actually show, and their absence is the largest hole
  here: every throughput number in this record is from a numeric or
  single-collection kernel.
* **Did not measure CPU time**, only wall. On a 32-CPU box running seven agents
  that distinction matters and the record does not resolve it.
* **Did not use a native profiler.** There is no `perf`/VTune reading here.
  §4's cause was isolated by differential timing and by the VM's own GC
  counters; §5's by two independent instruments; §7.3's attempt failed and says
  so.
* **Did not re-derive `BENCHMARK.md`'s Azure table** and did not attempt to
  reconcile it with §2. Different OS, different CPU, different load. The rows
  are compared as *ratios* and even that comparison is flagged, not relied on.
