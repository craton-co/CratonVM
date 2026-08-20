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

One term in `collect_roots`: G1 may not take the precise-only branch.

```rust
let moving_young_precise_only = moving_young
    && (!shared.mem.heap.is_g1() || g1_precise_only_roots)   // <- added
    && ...
```

ABBA-interleaved on azure host 2, one binary, `CRATONVM_G1_PRECISE_ONLY_ROOTS=1`
restoring the old behaviour:

```text
r1-fix PASS   r2-fix PASS   r3-fix PASS     precise_only=false scan_added=42
r1-ctl FAIL   r2-ctl FAIL   r3-ctl FAIL     precise_only=true  scan_added=0
```

This document already asserted that restriction as though it were implemented —
"`moving_young_precise_only` requires `is_generational()`, so under G1 it is
false" — and it was not. The added term is what makes the sentence true.

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

## What is still open

**The coverage proof's guarantee is narrower than the branch that spends it.**
`refresh_moving_young_coverage_for_collection` returned `true` for this stack —
`incomplete=false` in every arm — while `CRATONVM_DBG_VERIFY_OOP_MAPS` reports
in-band object addresses that no oop map names, on a compiled method whose
`fully_oop_covered` is `true`. (That oracle's own caveat is that the band it
walks may include nested-JIT-callee slots, so treat its count as an upper bound,
not a tally of missed oops. The 42-root A/B does not depend on it.)

The fix above stops **G1** depending on that proof. The generational
moving-young path still does. Whether the proof is wrong, or merely proves
something narrower than "every live reference on this stack is rewritable", is
the next question — and it is a different bug from this one.

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
