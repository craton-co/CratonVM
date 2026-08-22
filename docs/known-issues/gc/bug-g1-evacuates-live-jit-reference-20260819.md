# G1 evacuates a region holding a live JIT reference: `StringLatin1.newString`

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

## What is still open

**Is the codegen's coverage bit itself sound?** This pass did not answer that.
It established that the verifier which was supposed to check the bit could not
run on two of the three collectors, and stopped the two suppression paths from
depending on the unchecked answer. On the generational collector, where the
verifier does run, it reports `incomplete=true` on this workload — so the bit
was never load-bearing there either, and no run in this record has exercised
"verifier ran, verifier passed, collector moved".

`CRATONVM_DBG_VERIFY_OOP_MAPS` reports in-band object addresses no oop map
names, on a method whose `fully_oop_covered` is `true`. Treat that as an upper
bound, not a tally: the oracle builds its mapped set from ONE method's maps at
ONE frame base, so a nested callee's correctly-mapped slots count as unmapped.
Fixing that oracle to walk the RBP chain is the next concrete step if anyone
wants the real number.

**Two heaps at once still lose one.** Publication into `JIT_REGION_BOUNDS` is
last-writer-wins with no registry of live heaps, so constructing a second
generational heap leaves the first unrepresented. The fail-closed guard turns
that into a non-moving cycle rather than a silent vacuous pass, which is the
safe direction, but the table is still not a reliable answer to "is this address
young".

## Repro

```bash
cd /data/cratonvm/apps/bc-java
cratonvm --java-home <jdk25> -XX:+UseG1GC --Xmx 1g \
    -Dbc.test.data.home=<bc-test-data> -c "$(cat /data/bcjca-classpath.txt)" \
    junit.textui.TestRunner org.bouncycastle.pqc.math.ntru.test.PolynomialTest
```

~60-100s, deterministic. Add `CRATONVM_DBG_JIT_ROOTSCAN=1` for the line that
names the cause. On the pre-fix tree, `CRATONVM_JIT_DENY=java/lang/StringLatin1.newString`
turns it green, `--Xmx 8g` turns it green, and `-XX:+UseZGC` turns it green.

Drivers used for the measurements above: `/data/h2ki-{build,run,rate2,instr,c2,fix2,reg}.sh`.
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

(`ctl` = `CRATONVM_G1_PRECISE_ONLY_ROOTS=1`, which restores the pre-fix branch.)

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
