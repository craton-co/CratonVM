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

## Candidate fixes, in increasing ambition

1. **Refuse to evacuate when the publication cannot be trusted.** If
   `jit_active` and the pin set is empty, run the pause mark-only. Minimal,
   restores the generational collector's stance, and costs throughput on every
   JIT-triggered pause whose frame genuinely holds nothing — needs measuring
   before it is defaulted on.
2. **Make the conservative scan cover what it claims to.** Find why this frame's
   reference is not at an 8-byte aligned spill slot the scan walks. This is the
   real fix; the contract in `conservative_roots`'s header is the thing that is
   false.
3. Precise oop maps for JIT frames under G1, which the module header already
   names as the eventual answer.
