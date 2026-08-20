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

## The mechanism, from the collector's own instrument

`CRATONVM_G1_DBG_PINS=1` on the failing run. One young pause, and at it:

```text
[g1][PINS] young pause: jit_active=true pin_addrs=0 pin_regions={}
```

A JIT frame was active, **zero conservative JIT roots were published**, so
**zero regions were pinned**, so the collection set contained everything —
including the region holding the object compiled `newString` was using.

G1 implements the guard correctly (`g1.rs`, young-pause CSet build):

> Regions holding a conservatively-discovered JIT root must NOT be evacuated:
> the collector cannot rewrite the (register/spill) slot that holds the only
> reference, so the object must stay put (the generational collector achieves
> this by not moving anything while in JIT).

The guard is not wrong — it is **fed an empty set**. `conservative_roots`'s own
header states the contract it depends on:

> every real reference is at an 8-byte aligned spill slot — enforced by the JIT
> calling convention

For this compiled method that did not hold, so `scan_active_jit_frames`
returned nothing.

Note the branch: `moving_young_precise_only` requires `is_generational()`, so
under G1 it is false and the **conservative** path is the one that runs. This is
not the precise/shadow-stack path silently taking over; it is the conservative
scan finding nothing.

## Why this is a defect and not a tuning question

`pin_addrs=0` is being consumed as "there are no JIT roots". With
`jit_active=true` it actually means "the scan found none", which is **unknown**,
not **none** — the same silence-is-not-absence trap that has bitten this tree
before. An empty publication and a genuinely reference-free JIT frame are
indistinguishable at the point where the CSet is built, and only one of them is
safe to evacuate.

The generational collector avoids the whole question by not moving anything
while any thread is in JIT. G1 tries to move everything except what it was told
to pin, and is therefore exposed whenever the telling is incomplete.

## Repro

```bash
cd /data/cratonvm/apps/bc-java
cratonvm --java-home <jdk25> -XX:+UseG1GC --Xmx 1g \
    -Dbc.test.data.home=<bc-test-data> -c "$(cat bcjca-classpath.txt)" \
    junit.textui.TestRunner org.bouncycastle.pqc.math.ntru.test.PolynomialTest
```

~100s, deterministic. `CRATONVM_JIT_DENY=java/lang/StringLatin1.newString`
turns it green; `--Xmx 8g` turns it green; `-XX:+UseZGC` turns it green.

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
