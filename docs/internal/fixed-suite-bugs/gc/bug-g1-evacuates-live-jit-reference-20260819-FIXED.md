# G1 evacuates a region holding a live JIT reference: `StringLatin1.newString`

## Status

**FIXED and RETIRED 2026-09-01.** The two fixes this page shipped
(`collect_roots`' collector term, and the fail-closed bounds guard) are still in
and still load-bearing; both residuals it closed with are closed too, and the
repro is green on all three collectors on the current tip.

| residual, as this page left it | closed by |
|---|---|
| "the oracle builds its mapped set from ONE method's maps at ONE frame base … fixing that oracle to walk the RBP chain is the next concrete step if anyone wants the real number" | the oracle walks the RBP chain (landed before this pass); **the number is measured below** |
| "no run in this record has exercised *verifier ran, verifier passed, collector moved*" | measured below — 11 of 11 generational collections, `ybounds=true reason=none`, green |
| "two heaps at once still lose one … the table is still not a reliable answer to *is this address young*" | a live-heap registry: the gate refuses while the process holds more heaps than a single-tenant table can describe |
| "the counter is ungated and should now read zero" (`[GC] g1 jit publication`) | it was ungated and **unprinted** — the report had no production caller, then had one on the exit arm this workload never takes. Both fixed. |


## The symptom

`org.bouncycastle.pqc.math.ntru.test.PolynomialTest` fails under G1, and only
under G1:

```text
testS3FromBytes(...PolynomialTest)
java.util.IllegalFormatConversionException: d != java.lang.Object
```

`%d` was handed a bare `java.lang.Object`. Found by the first 53-class sweep run
per collector (bug-bcjava-53class-gc-sweep-20260819.md); the default collector
cannot reach it.

## The conditions, each independently controlled

| condition | result |
|---|---|
| G1 + JIT + `--Xmx 1g` | **FAIL** (deterministic, 3/3 in the sweep + every run below) |
| ZGC, any JIT setting | PASS |
| Generational | PASS |
| G1 + `--nojit` | PASS |
| G1 + JIT + **`--Xmx 8g`** | **PASS** |
| G1 + JIT + `--Xmx 1g` (repeat) | FAIL |

So it needs **G1** (the only moving collector of the three here), **the JIT**,
and **GC to actually run** — an 8g heap makes it disappear because the young
pause never happens.

## Which compiled method

`CRATONVM_JIT_DENY` bisect, each step one run:

```text
java/                             -> PASS      java/lang/                 -> PASS
java/lang/String                  -> PASS      java/lang/StringLatin1     -> PASS
java/lang/StringLatin1.newString  -> PASS
```

and the negative side, to show the split is real:

```text
java/util/, java/util/Formatter               -> FAIL
java/lang/Integer, java/lang/StringBuilder    -> FAIL
java/lang/StringUTF16, StringConcatHelper     -> FAIL
java/lang/StringLatin1.toBytes                -> FAIL
java/lang/StringLatin1.equals   (alone)       -> FAIL
java/lang/StringLatin1.hashCode (alone)       -> FAIL
java/lang/NoSuchClassZZZ  (control)           -> FAIL
```

The control matters: denying a class that does not exist still fails, so it is
not "denying anything" that fixes it — it is denying **`newString`**.

`StringLatin1.newString(byte[], int, int)` is what `String.substring` calls, so
it is on `Formatter`'s format-string parsing path. It allocates
(`Arrays.copyOfRange`), which is the GC point.

## The mechanism

`CRATONVM_G1_DBG_PINS=1` on the failing run. One young pause, and at it:

```text
[g1][PINS] young pause: jit_active=true pin_addrs=0 pin_regions={}
```

Zero regions pinned, so the collection set contained everything — including the
region holding the object compiled `newString` was using.

G1 implements the guard correctly (`g1.rs`, young-pause CSet build):

> Regions holding a conservatively-discovered JIT root must NOT be evacuated:
> the collector cannot rewrite the (register/spill) slot that holds the only
> reference, so the object must stay put (the generational collector achieves
> this by not moving anything while in JIT).

The guard is not wrong — it is **fed an empty set**. The question is why, and
`pin_addrs=0` cannot answer it: that number is reported where the collection set
is built, downstream of every way it can reach zero. `CRATONVM_DBG_JIT_ROOTSCAN=1`
prints the inputs, one line per collection, at the point the pins are published:

```text
[jitroots] precise_only=true moving_young=true osr_fb=false incomplete=false
           chain=2 any_jit=true scan_added=0 is_g1=true
           frames=["...PolynomialTest.testS3FromBytes:()V",
                   "java/text/DecimalFormatSymbols.getInstance:()..."]
```

**The conservative scan was never called.** `memory::roots::collect_roots` skips
it whenever the oop-map coverage proof passes:

```rust
let moving_young_precise_only = moving_young
    && !moving_young_osr_fallback
    && refresh_moving_young_coverage_for_collection()
    && !moving_young_coverage_incomplete();
if !moving_young_precise_only {
    scan_active_jit_frames(&shared.mem.heap, &mut roots);
}
```

and G1's pin set is built, a few lines below, from that scan's output alone:

```rust
if shared.mem.heap.is_g1() && roots.len() > jit_scan_start {
    for r in &roots[jit_scan_start..] { add_pinned_jit_root(r.as_ptr() as usize); }
}
```

### Why that is sound for one collector and not the other

The two collectors protect a JIT-held oop by opposite means.

*Precise-only is sound for a collector whose protection is REWRITING.* The proof
says every JIT-held oop sits in a published oop map; the collector moves the
object and `remap_active_jit_frames` writes the new address back into the slot
the map named. The generational collector is that collector.

*G1's protection is PINNING.* Skipping the scan does not leave G1 with a
different protection — it leaves it with **none**, for every reference the maps
do not happen to name. `pin_addrs=0` then reads as "there are no JIT roots" and
is consumed as permission to evacuate everything.

### The A/B

`CRATONVM_MOVING_YOUNG_NO_JIT=1` forces the conservative path. Same binary:

| arm | `precise_only` | `scan_added` | result |
|---|---|---|---|
| default | `true` | 0 | FAIL 2/2 |
| `MOVING_YOUNG_NO_JIT=1` | `false` | **42** | **PASS 3/3** (`OK (14 tests)`) |

Forty-two conservative roots on a stack the coverage proof called complete
(`incomplete=false` in both arms).

The same predicate separates the two workloads that looked alike from the
collector's side:

| workload | `precise_only` | `scan_added` | `pin_addrs` |
|---|---|---|---|
| `MovingYoungConcurrentProbe 6 400 2000`, 256m | `false` | 14 | 18 |
| `PolynomialTest`, 1g | `true` | 0 | 0 |

The probe never trips this at all: its coverage proof does not pass, so it takes
the conservative path and gets pinned normally.

## The fix

One term in `collect_roots`: only the generational collector may take the
precise-only branch.

```rust
let moving_young_precise_only = moving_young
    && (shared.mem.heap.is_generational() || g1_precise_only_roots)   // <- added
    && ...
```

Spelled `is_generational()`, not `!is_g1()`, because that is what the two
SIBLING suppression sites already say — see "Both fixes are independently
sufficient" below. It additionally excludes ZGC, which publishes no young-bounds
table either and so also receives a vacuous proof.

ABBA-interleaved on azure host 2, one binary, `CRATONVM_G1_PRECISE_ONLY_ROOTS=1`
restoring the old behaviour:

```text
r1-fix PASS   r2-fix PASS   r3-fix PASS     precise_only=false scan_added=42
r1-ctl FAIL   r2-ctl FAIL   r3-ctl FAIL     precise_only=true  scan_added=0
```

This document already asserted that restriction as though it were implemented —
"`moving_young_precise_only` requires `is_generational()`, so under G1 it is
false" — and it was not. The added term is what makes the sentence true.

### Verification

Beyond the ABBA arms above, on azure host 2, `-XX:+UseG1GC --Xmx 1g`, JIT on:

| check | result |
|---|---|
| `-XX:+UseGenerationalGC`, fix vs pristine base | PASS / PASS — see "What the collectors actually do"; the ntru test was never red there |
| 8 `bcjava-pass-list.txt` classes under G1, fix vs base | **8/8 identical**, same test counts (24, 17, 7, 19, 27, 1, 177, 1) |
| `cargo test -p cratonvm-vm --lib` | 2576 passed, 0 failed |
| `cargo test -p cratonvm-gc --lib` | 1684 passed, 0 failed |

Two harness faults are recorded rather than deleted, because both produced rows
that READ as evidence:

* an earlier collateral attempt used
  `org.bouncycastle.{crypto,util}.test.RegressionTest`, which are not JUnit-3
  suites — both arms reported `No tests found` and FAILED identically, which
  reads as "no regression" and was "no test";
* the first pass's control arms were labelled "generational" and were **ZGC**
  runs, because a default build selects ZGC and
  `GcAlgorithm::Generational`'s doc comment claimed otherwise. A control arm
  named after a collector it did not run is worse than no control arm.

## Two things the earlier reading got wrong

Both are recorded because each cost a wrong fix, and both were resolved by an
instrument rather than by more reading.

**1. "It is the conservative scan finding nothing."** This record said so
explicitly, and ruled out the alternative in the same breath: "This is not the
precise/shadow-stack path silently taking over." It was exactly that. The claim
was derived from reading the branch, not from running it; `precise_only=true` is
one line of output away and disagrees.

**2. Candidate 1 (refuse to evacuate on an empty publication) was built and
measured, and it is not the fix.** It stops the corruption — the format
exception disappears in 7/7 runs and returns when the lever is flipped in the
same binary — but because the empty publication was the ordinary appearance of
precise mode, refusing meant refusing *every in-JIT pause*: 1444 consecutive
no-op pauses and `OutOfMemoryError` on 8 tests, where the unrefused run gave a
wrong answer on 1. That is the same wall `CRATONVM_G1_COVERAGE_PIN` hits at its
~99.99% rate, reached from a different predicate, and for the same underlying
reason: **G1 has no non-moving reclamation path, so "decline to evacuate" always
means "reclaim nothing".**

The detector is kept (`G1Collector::empty_jit_publication`,
`CRATONVM_G1_PIN_EMPTY_PUBLICATION`, default OFF) because the fix above changes
what it measures. With the conservative scan always running under G1, an empty
publication under a live compiled frame is once again what its name says — the
scan ran and validated nothing — and that is a genuine anomaly. The counter is
ungated and should now read zero:

```text
[GC] g1 jit publication: pauses=N empty_while_in_jit=K (y%)
```

## Why the coverage proof passed: the verifier was vacuous

The proof is not merely trusting the codegen's `moving_young_coverage_complete`
bit. `refresh_moving_young_coverage_for_current_thread` also runs a frame-band
verifier — `moving_young_unpublished_frame_oop_present` — whose module block
states exactly the defect this record is about:

> A compiled frame holds oops in storage the abstract model does not describe:
> SCALAR-REPLACED OBJECT FIELDS ... LICM HOIST SLOTS ... THE FULL-GPR SAFEPOINT
> SPILL AREA. On the non-moving path all three are covered, because the
> conservative frame scan reads every word of the frame. Under moving-young
> `memory/roots.rs` SUPPRESSES that scan when the coverage proof passes.

and which claims to remove the need to trust the bit. It is default-on.

**It could not have found anything under G1.** Its residency test is
`gen_heap::addr_in_published_young_regions`, which reads `JIT_REGION_BOUNDS`.
That table has exactly one writer, `store_region_bounds_locked`, and it is
generational-only — G1 leaves it empty deliberately (publishing it would make an
inline reference-STORE fast path reachable and cost a CSet-excluded region its
remembered-set edge, the G1-2 decision, stated in the `G1Collector`
constructor), and ZGC fills neither table. Where the table is empty the test
answers `false` for **every address in the process**, so the verifier walks every
verifiable slot, classifies none of them as young, and returns "nothing
unpublished" without having inspected anything.

So the chain is: empty table → vacuous verifier → `incomplete=false` →
`roots.rs` suppresses the conservative scan → G1's pin set is empty →
`pin_addrs=0` → evacuate. A predicate that is false everywhere reads as absence.

### The fail-closed fix

The verifier already reports "not verified" for an unbounded band, a missing
frame size, or an unresolvable shadow window. An unpublished residency table is
the same class of "cannot inspect" and was the one case not failing closed. It
does now, under a `YOUNG_BOUNDS_UNPUBLISHED` reason so the fallback histogram
separates it from a real missed oop.
`CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD=1` restores the vacuous pass.

### Both fixes are independently sufficient

Measured as a 2x2 in one binary, `-XX:+UseG1GC --Xmx 1g`, two rounds each:

| collector term | bounds guard | `precise_only` | `incomplete` | result |
|---|---|---|---|---|
| on | on | false | false | PASS |
| **off** | on | false | **true** | PASS |
| on | **off** | false | false | PASS |
| **off** | **off** | **true** | false | **FAIL** |

The two reach the same place by different routes — the guard makes the proof
fail closed, the collector term declines to act on a proof that still passes
vacuously — and they are not redundant, because the proof has **three**
consumers. `interpreter::update_root_snapshot` and
`vm_exec::deposit_root_snapshot` suppress their own conservative scans on it
too, and the guard is what fixes those; the collector term only fixes
`collect_roots`.

That third point also reframes the collector term. Both sibling sites already
open with `heap.is_generational() && moving_young_enabled() && <proof>`.
`collect_roots` was the one that had lost the term. It is not a new restriction,
it is a drift repair, and the term is now spelled the same way as its siblings.

## A second, independent way the table goes empty

`Drop for GenerationalHeap` cleared both global tables **unconditionally**, so a
heap going away wiped bounds a still-live heap had published. That was tolerable
while the only reader was the JIT's inline getfield guard, where an empty table
costs a helper call; it stopped being tolerable when a correctness predicate
started reading the same table. Fixed: only the publisher clears, discriminated
by the young-from base in slot 0. Unit-tested both ways.

**Not** what made the table empty in the runs above — those were G1 and ZGC,
which never publish it at all. Recorded because the mechanism is real and was
found while chasing this, not because it explains this failure.

## What the collectors actually do

Measured with `CRATONVM_DBG_JIT_ROOTSCAN=1`, all fixes in:

| collector | `ybounds` | `precise_only` | scan roots | ntru |
|---|---|---|---|---|
| G1 (`-XX:+UseG1GC`) | false | false | 42 | PASS |
| default = **ZGC** | false | false | 22-36 | PASS |
| `-XX:+UseGenerationalGC` | **true** | false | 33-49 | PASS |

Two things to read off it. The generational collector publishes the table, so
the guard is inert there — and it was already declining the precise-only path on
this workload for other obligations (`incomplete=true` on 8 of 8 collections),
so it loses nothing.

And **the default collector is ZGC, not generational.** `VmConfig::default`
selects `Zgc` whenever the `zgc` feature is on, which the release build has;
`GcAlgorithm::Generational`'s doc comment claimed to be the default and was
stale (corrected in the same change). Three control arms in the first pass of
this investigation were recorded as "generational" and were ZGC runs.

### Cost on the default collector

The collector term makes ZGC always run the conservative scan, where the vacuous
proof previously let it skip. ABBA, pristine base vs fix, default collector:

| class | base | fix |
|---|---|---|
| `crypto.hash2curve.test.AllTests` (177 tests) | 70249 / 67818 ms | 68851 / 68735 ms |
| `asn1.test.AllTests` (24 tests) | 9135 / 12820 ms | 11473 / 6496 ms |
| `crypto.agreement.test.AllTests` (27 tests) | 1510 / 1505 ms | 1690 / 1580 ms |

12 of 12 PASS with identical test counts. No regression is detectable, and the
honest limit of that statement is the host: the two short classes vary by 2x
run-to-run, so only the 70 s class resolves anything, and there the fix sits
inside base's own spread. The mechanism agrees — the scan runs per collection
and these workloads take single-digit collections.

## The two residuals, and how each closed

### 1. Is the codegen's coverage bit sound? — measured, 2026-09-01

The page left this open with a concrete next step: the
`CRATONVM_DBG_VERIFY_OOP_MAPS` oracle "builds its mapped set from ONE method's
maps at ONE frame base, so a nested callee's correctly-mapped slots count as
unmapped", and "fixing that oracle to walk the RBP chain is the next concrete
step if anyone wants the real number".

The oracle was fixed — it walks the RBP chain exactly as
`scan_compiled_frame_bands` does, checks each frame against ITS OWN method's
maps at ITS OWN rbp over its own band only, and splits the hits into
`never_mapped` / `wrong_map` / `below_jit`. Nobody had run it. Here is the
number: `PolynomialTest`, `--Xmx 1g`,
`CRATONVM_DBG_JIT_ROOTSCAN=1 CRATONVM_DBG_VERIFY_OOP_MAPS=1`. The three
single-collector rows are one binary; the second G1 row is the build one commit
earlier, whose only difference is an unrelated stderr print — stated rather than
elided, because a cross-binary row is not an A/B row and this page has been
burnt by one before.

| collector | colls | `ybounds` | `reason` | frames | verifiable words | `never_mapped` | `while_covered` | result |
|---|---:|---|---|---:|---:|---:|---:|---|
| G1 (`-XX:+UseG1GC`) | 1 | false | young-bounds-unpublished | 5880 | 90 716 | **0** | 0 of 3898 | OK (14 tests) |
| G1, a second run | 1 | false | young-bounds-unpublished | 5884 | 90 772 | **6** | 4 of 3900 | OK (14 tests) |
| generational | 11 | **true** | **none** | 438 | 5 583 | **0** | 0 of 314 | OK (14 tests) |
| ZGC (default) | 2 | false | coverage-oracle-refuted, none | 5882 | 90 736 | **2** | 2 of 3900 | OK (14 tests) |

Three things to read off it, and one of them answers the other half of the
residual.

**The count is 0, 6, 0, 2 across four runs.** A codegen gap does
not come and go: `String.trim`'s compiled body either names a slot in its maps
or it does not, and it runs thousands of times per run. A count that is zero on
one G1 run and six on the next is measuring whether an address-shaped word
happened to point at a live object at the moment of a pause — the ONE direction
this oracle is documented to be conservative in ("a primitive `i64` whose bits
land on a live object header is counted"). So a zero here is the strong result
and the non-zeros are leads, exactly as its own header says.

**The independent oracle cannot adjudicate them, and that is structural rather
than an oversight.** Every hit classifies as `verifier_unknown`
(`verifier_oop=0 verifier_not_oop=0`), and all the distinct sites across the two
non-zero runs are in the `operand-spill` band:

```text
rbp-0x40 operand-spill sp_id=11 oop_cov=true  java/lang/String.trim:()Ljava/lang/String;
rbp-0x50 operand-spill sp_id=68 oop_cov=true  java/lang/StringLatin1.trim:([B)Ljava/lang/String;
rbp-0x68 operand-spill sp_id=16 oop_cov=false java/lang/StringLatin1.newString:([BII)Ljava/lang/String;
```

`verifier_local_verdict` refuses any slot that is not a java local and says why
in as many words: "Operand spill is `Frame::stack` indexed by runtime depth,
which this frame does not carry." That is not a gap waiting to be filled. The
single-pass backend hands out spill offsets **per Frame push, not per stack
position** — `canonicalize_stack` exists precisely because the two disagree
between merge points, and a register-resident stack entry occupies a position
with no frame slot at all. An offset→depth mapping would therefore be right at
merge points and silently wrong everywhere else, which is the same
"reads a different thing at a coincidentally-valid index" error the module
already refuses to make for an inlined frame.

So the honest answer to "is the bit sound?" is: **on the region where the hits
land, the available independent oracle cannot say — and the design no longer
depends on it.** The suppression the bit licenses is opt-in
(`CRATONVM_GC_PRECISE_ONLY_ROOTS=1`, default OFF since 2026-08-21 and worth
~0.1 % of collections; `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820.md`),
so the conservative band scan runs on every collection and covers exactly the
operand-spill words the oracle flags. The ZGC row shows the one remaining
dependence behaving correctly: two unadjudicable hits latched
`COVERAGE_ORACLE_REFUTED`, and that cycle declined to relocate. One
non-relocating cycle is what an unprovable claim costs, and it is the safe
direction.

**"Verifier ran, verifier passed, collector moved" is now exercised.** The page
recorded that no run in it had ever reached that state, because on the only
collector whose residency table is published the verifier reported
`incomplete=true` on 8 of 8 collections. The generational row above IS that
state: `ybounds=true` (the table is published, so the verifier is not vacuous),
`reason=none` on **11 of 11** collections, and the run is green.

That is a change in the verifier's verdict, not in the question asked of it:
`refresh_moving_young_coverage_for_collection` is the same call with the same
obligations whether or not the suppression is on. Something between 2026-08-19
and now removed the obligation this workload was failing, and the likeliest
candidate is named in the sibling page — the coverage bit "asserted that a map
EXISTS per safepoint, not that the map lists every live oop, and the thing it
failed to describe was the staged invoke-argument buffer", which is a codegen
gap that would report exactly as `UNPUBLISHED_FRAME_OOP` here. Not chased
further: the reading that matters is that the state the page said had never been
exercised now is, 11 times in one run, green.

### 2. Two heaps at once still lose one — fixed

This is the same vacuous pass the whole page is about, reached from the side no
check could see.

`JIT_REGION_BOUNDS` and `MOVABLE_BOUNDS` are process-global and discriminated by
slot 0 — a table "belongs to" whichever heap's base it currently names, which is
what makes the owner-checked clears in the three `Drop`s correct. That
discrimination has a consequence the clears do not: **a table can describe
exactly one heap.** Construct a second and the first is unrepresented in it.

That was tolerable while the only reader was the JIT's guarded inline
`getfield`, where an unrepresented arena costs a helper call. It stopped being
tolerable the moment
`conservative_roots::moving_young_unpublished_frame_oop_present` began asking
`gen_heap::addr_is_movable` whether a compiled frame's word could be relocated:
for the displaced heap that predicate answers `false` for **every address in the
process**, so the verifier walks every verifiable slot, classifies none of them
as movable, and returns "nothing unpublished" having inspected nothing.

What makes it worse than the empty-table case is that it looks like success from
every angle the empty-table case is checked from. `movable_bounds_published()`
is true. The values are fresh. The publisher is live. `movable_bounds_are_live()`
— the fail-closed gate this page added — returned **true**. Only the number of
live heaps separates the two states, and nothing counted it.

**The fix is a live-heap registry.** `RelocatableHeapRegistration` is an RAII
field on `GenerationalHeap`, `ZgcRealHeap` and `G1Collector`;
`movable_bounds_are_live` refuses while more heaps are alive than a
single-tenant table can describe. RAII rather than a matched pair of calls, so
an unwind out of a half-built heap cannot leak a count that would disable
moving-young for the life of the process.

Widening the tables to hold N heaps was the alternative and is the wrong trade:
their address is baked into compiled code as an immediate and their six-word
shape is what the emitted containment sequence walks, so a variable-length table
would put a loop bound on the JIT's hottest guard to serve a case a running VM
never has — `vm_init` builds exactly one heap. Keeping the fast shape and
REFUSING to answer when it cannot describe the process is what every other
"cannot inspect" case in this verifier already does.

G1 registers too, though it publishes into neither table. On its own it already
fails the verifier closed for want of any published range; the registration is
what keeps a MIXED process honest, where another collector's published table
would otherwise be read as an answer about G1's addresses.

Reported apart from the empty-table case, because the operator action differs.
`YOUNG_BOUNDS_UNPUBLISHED` says a collector publishes nothing and is expected
under G1; the new `BOUNDS_NOT_REPRESENTATIVE` says the process holds more heaps
than the tables can describe, which no production configuration does. And
`[jitroots]` grew `heaps=` and `bounds_representative=` beside `ybounds=`,
because `ybounds=true heaps=2` is the reading neither number gives alone.

Two tests in `gc/tests/published_bounds_isolation.rs` — the file whose own
header invited them ("the identity-scoped form stays correct if this file ever
grows a test that builds two heaps at once"). Verified by breaking it: drop the
registry term from `movable_bounds_are_live` and
`a_second_live_heap_makes_the_published_bounds_gate_fail_closed` fails while
every unrelated test in the file still passes.

## A third thing was wrong, and only the retirement pass looked

The page keeps `G1Collector::empty_jit_publication` as a detector rather than a
fix, on the grounds that with the conservative scan always running under G1 an
empty publication under a live compiled frame is once again what its name says.
It then states:

> The counter is ungated and should now read zero:
> `[GC] g1 jit publication: pauses=N empty_while_in_jit=K (y%)`

Ungated it was. **Printed it was not.** `gc_metrics::collector_decision_report`
— which carries that line, the sibling `g1 root coverage` line, the decision
histogram and the per-reason moving-young fallback rows — had no caller outside
its own unit tests. Nothing a running VM does has ever emitted it.

Giving it a caller was not enough either. The first attempt put it beside
`print_gc_summary` in the launcher's teardown, which is the NORMAL-RETURN arm —
and this page's own repro ends in `System.exit`, because `junit.textui.TestRunner`
does. The report moved from "no caller" to "a caller on the arm this workload
never takes", which prints the same nothing. It is emitted from
`maybe_dump_shutdown_reports` now, the function W7-90 created for exactly this
shape of mistake, which runs on both arms and is latched so it runs once.

The lesson is this page's own, one level up: a counter whose report nothing
prints reads as zero for the same reason a predicate that is false everywhere
reads as absence.

## Re-verified 2026-09-01, and the A/B is on the MECHANISM now

Azure host 2, one binary (`cratonvm-g1oop`, md5 `769981026689…`), G1, JIT on,
`junit.textui.TestRunner org.bouncycastle.pqc.math.ntru.test.PolynomialTest`.
The control arm restores the whole pre-fix predicate — the suppression
(`CRATONVM_GC_PRECISE_ONLY_ROOTS=1`, opt-in since 2026-08-21), G1's permission to
take it (`CRATONVM_G1_PRECISE_ONLY_ROOTS=1`), and the vacuous proof that fed it
(`CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD=1`).

### Scored on the unsound STATE, not on the wrong answer

The page's original arms were scored `PASS`/`FAIL` on
`IllegalFormatConversionException`, and called the failure deterministic. It is
not: `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820.md` measured
the same signature at **2 of 6** and load-sensitive. Six control runs here, at
`--Xmx 256m` to get six young pauses each instead of one, are green on the
symptom — and the *state* the symptom comes out of is present on every single
pause:

| arm | runs | collections | `precise_only=true` | pauses with an EMPTY JIT publication | junit |
|---|---:|---:|---:|---:|---|
| fix (default) | 6 | 37 | **0** | **0** | 6/6 `OK (14 tests)` |
| control | 6 | 36 | 32 | **31** | 6/6 `OK (14 tests)` |

ABBA-interleaved (`a1-fix a1-ctl b1-ctl b1-fix a2-fix …`), one binary. Every
pause the control skips the conservative scan on evacuates against a root set it
cannot prove; the fix takes that branch zero times in 37 collections.

That is the better instrument, and the reason is worth keeping: the corruption
needs the evacuated region to be the one a live compiled frame still references,
which is a coincidence per pause. The *unproven root set* is not a coincidence —
it is deterministic given the predicate. A/B on the mechanism and the signal is
31 of 32; A/B on the symptom and it is 0 of 6 on a quiet enough host, which reads
as "no bug".

### The counter that "should now read zero" does, and can fire

Same binary, same workload, `--Xmx 256m` (six pauses in both arms), the two
lines this page named — now that anything prints them at all:

```text
fix        [GC] g1 root coverage:   pauses=6 incomplete=6 (100.00%)
           [GC] g1 jit publication: pauses=6 empty_while_in_jit=0 (0.00%)

control    [GC] g1 root coverage:   pauses=6 incomplete=0 (0.00%)
           [GC] g1 jit publication: pauses=6 empty_while_in_jit=6 (100.00%)
```

Both lines invert together, which is this page's whole chain in two numbers: the
control's coverage proof passes on every pause (`incomplete=0`) **because** it is
vacuous, and every pause then publishes nothing to pin (`empty_while_in_jit=6`).
The fix's proof fails closed on every pause and nothing is ever unpinned.

The zero is worth having only because the control shows the same counter reading
100 % in the same binary. A zero from an instrument that cannot fire is what the
first half of this page is about.

### Everything else

| check | result |
|---|---|
| `PolynomialTest`, G1 / generational / ZGC, `--Xmx 1g` | `OK (14 tests)` on all three |
| `PolynomialTest`, HotSpot 25 (classpath control) | `OK (14 tests)` |
| `[jitroots]` in every arm | `heaps=1 bounds_representative=true` — the registry is inert on the one-heap shape every production process has |
| `cargo test -p cratonvm-gc --lib` | 1704 passed, 0 failed |
| `cargo test -p cratonvm-gc --tests` | all integration targets pass, incl. 8/8 `published_bounds_isolation` |
| `cargo test -p cratonvm-vm --lib` | 2644 passed, 0 failed |
| `cargo check --workspace --all-targets` | clean |

## Repro

```bash
cd /data/cratonvm/apps/bc-java
cratonvm --java-home <jdk25> -XX:+UseG1GC --Xmx 1g \
    -Dbc.test.data.home=<bc-test-data> -c "$(cat /data/bcjca-classpath.txt)" \
    junit.textui.TestRunner org.bouncycastle.pqc.math.ntru.test.PolynomialTest
```

~60-100s. Add `CRATONVM_DBG_JIT_ROOTSCAN=1` for the line that names the cause.
On the pre-fix tree, `CRATONVM_JIT_DENY=java/lang/StringLatin1.newString`
turns it green, `--Xmx 8g` turns it green, and `-XX:+UseZGC` turns it green.

"deterministic", which this line said, is wrong and was corrected by the sibling
page: the wrong ANSWER is load-sensitive (2 of 6 there, 0 of 6 here on a quieter
host). What is deterministic is the unsound STATE — see "Scored on the unsound
STATE" above, and score on `empty_while_in_jit`, not on the exception.

### Rebuilding the repro from scratch, 2026-09-01

`/data/bcjca-classpath.txt` and the `/data/h2ki-*.sh` drivers this page cites are
gone from the host. Both are one command away. The classpath is the bc-java
gradle build's own output directories plus its junit jar:

```bash
BC=/data/cratonvm/apps/bc-java
CP=$BC/core/build/classes/java/main:$BC/core/build/classes/java/test
CP=$CP:$BC/prov/build/classes/java/main:$BC/prov/build/classes/java/test
CP=$CP:$BC/util/build/classes/java/main:$BC/pkix/build/classes/java/main
CP=$CP:$BC/core/src/test/resources:$BC/prov/build/resources/main
CP=$CP:$BC/libs/junit-4.13.2.jar
```

and the data home is `/data/cratonvm/apps/bc-test-data`. Validate it with
HotSpot first — `jdk-25/bin/java` on that classpath gives `OK (14 tests)` in
~2 s, which separates "the classpath is wrong" from "the VM is wrong" before a
single VM arm runs.

Two host facts that cost time here, recorded so they do not cost it again: the
shared cargo registry under `/data/toolchain/cargo` had an incompletely
extracted crate (`cc-1.2.58` missing `src/target/`, which reads as a compiler
error in a dependency and is repaired by re-extracting that path from the
`.crate` in the sibling `cache/` directory), and the host's ROOT filesystem
runs at 100 % — a release build needs `TMPDIR` pointed at `/data` or `cc` fails
with "No space left on device" while compiling bundled sqlite. Build only what
is needed (`-p cratonvm-cli --bin cratonvm`), and at `-j 1`..`-j 3`: the
fat-LTO link was SIGKILLed by the OOM killer at load 45.
## Two fixes were tried. Both are refuted, and that narrows the third.

### Attempt 1 — refuse to evacuate when the publication is empty: OOMs

Implemented as a default-ON kill switch (`CRATONVM_GC=-jit-safe-cset`): when
`jit_active` and `pinned_jit_root_count() == 0`, build an empty CSet and let the
pause run mark-only, which is the generational collector's stance.

It does remove the wrong answer — `IllegalFormatConversionException` is gone —
and the kill switch flips it straight back, so the guard is demonstrably what
changed the behaviour. But it replaces one failure with another:

```text
Tests run: 14,  Failures: 0,  Errors: 8
java.lang.OutOfMemoryError: Java heap space (new_object class_id 64 fields 18)
```

**Not shippable.** In this workload almost every young pause has a JIT frame
active and an empty publication, so "skip evacuation" means "never evacuate",
and a 1g heap fills. Trading a wrong answer for an `OutOfMemoryError` is not a
fix. The change was reverted; it is recorded here because the *correctness* half
of it is a clean positive control for the mechanism.

### Attempt 2 — widen the conservative scan: does not help

`CRATONVM_DBG_FULLSTACK_SCAN` already exists for exactly this question. Its own
comment states the decision rule:

> if this makes a live object visible … the missed root WAS on the stack but
> outside the JIT chain's bounds (a range bug); if corruption persists, the
> missed root is not on the stack at all

Scanning the **entire** native stack instead of the per-entry
`[scanner_sp, entry_sp)` ranges changes nothing — same failure, twice, and the
pause still logs `pin_addrs=0`:

```text
PLAIN            FAIL   empty-pin pauses: 1
FULLSTACK        FAIL   empty-pin pauses: 1
FULLSTACK_AGAIN  FAIL   empty-pin pauses: 1
```

So **this is not a scan-range bug**, and widening the conservative scan cannot
reach the reference. By the flag's own rule the missed root is not on the native
stack at all — it lives in a callee-saved register (or is materialisable only
from one) at the moment of the pause, which is precisely the case conservative
stack scanning cannot cover and `pin_addrs=0` was faithfully reporting.

That `pin_addrs` stays 0 even with the whole stack scanned is worth stating
plainly: the publication is not dropping roots it found, it is finding none,
because there are none *to find on the stack*.

### What is left

**Precise oop maps for JIT frames under G1** — which `conservative_roots`'s
header already names as the eventual answer and which the two experiments above
now leave as the only candidate that can work. Until then G1 remains exposed on
any compiled frame that keeps its only reference in a register across an
allocation, and the practical mitigations are the ones already measured:
`-XX:+UseZGC` (the default), `--nojit`, or a heap large enough to avoid the
pause.

A narrower interim option worth measuring, if G1 correctness is wanted before
oop maps land: make the empty-publication guard **evacuate but bound the CSet**
rather than skip it entirely — e.g. keep evacuating regions that no thread could
have referenced since the last safepoint — so the heap still drains. That is a
design question, not a patch.

## Attempt 2's instrument could not fire, so its conclusion does not hold

Attempt 1 above is confirmed independently — the same guard, built separately,
produced the same `Tests run: 14, Errors: 8` OOM. It ships as an opt-in lever
(`CRATONVM_G1_PIN_EMPTY_PUBLICATION`, default OFF), not as a fix.

Attempt 2 is a different matter. `CRATONVM_DBG_FULLSTACK_SCAN` has **exactly one
reader**, at the top of `conservative_roots::scan_active_jit_frames`:

```console
$ grep -n 'dbg_fullstack_scan()' vm/src/jit/conservative_roots.rs
3639:    if dbg_fullstack_scan() {
```

And `scan_active_jit_frames` is the call `collect_roots` **skips** whenever
`moving_young_precise_only` holds — which is exactly the state this bug occurs
in. So on the failing run the flag was read by a function that never executed.
The three arms were not three experiments; they were the same run three times,
which is why they report an identical `empty-pin pauses: 1`.

The flag's decision rule is sound, but it was never applied: neither branch of
"if this makes the object visible … if corruption persists …" was actually
tested. See `a-narrow-probe-reports-its-own-reach-not-the-defect`.

### What the scan finds when it is allowed to run

`CRATONVM_DBG_JIT_ROOTSCAN=1`, one line per collection:

```text
precise_only=true   incomplete=false  chain=2  scan_added=0    <- as shipped: skipped
precise_only=false  incomplete=false  chain=2  scan_added=42   <- forced to run
```

Forty-two conservative roots on the stack the previous section concluded had
none. The reference is reachable to a conservative scan because
`safepoint_reg_spill_all` blind-spills every used callee-saved GPR into a frame
slot before each GC-capable call — so a register-resident oop *is* on the stack
by the time the pause happens. That is the mechanism the "callee-saved register"
reading missed.

End-to-end, ABBA-interleaved, one binary, flag flipped:

```text
r1-fix PASS   r2-fix PASS   r3-fix PASS      precise_only=false  scan_added=42
r1-ctl FAIL   r2-ctl FAIL   r3-ctl FAIL      precise_only=true   scan_added=0
```

(`ctl` = `CRATONVM_G1_PRECISE_ONLY_ROOTS=1`, which restored the pre-fix branch
**at the time this was measured**. It no longer does on its own: since
2026-08-21 the suppression is itself opt-in, so a pre-fix control now needs
`CRATONVM_GC_PRECISE_ONLY_ROOTS=1` and `CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD=1`
alongside it — all three, or the arm silently does not engage and reads as
"the control passed". See "Re-verified 2026-09-01" above.)

So precise oop maps were **not** the only remaining option. Two things shipped
instead, and the bug is closed:

1. **G1 no longer takes the precise-only branch** (`collect_roots`). The two
   collectors protect a JIT-held oop by opposite means — the generational one by
   rewriting what the maps name, G1 by pinning what the conservative scan
   publishes. Skipping that scan left G1 with no protection rather than a
   different one.
2. **The coverage proof itself was unsound**, and is now fixed: it asserted a map
   EXISTS per safepoint, not that the map lists every live oop, and the thing it
   failed to describe was the staged invoke-argument buffer. Recorded separately
   in `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820.md`.

The interim option floated above — "evacuate but bound the CSet" — is not needed.
