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

## Candidate 1 is measured, and it does not work

> **Refuse to evacuate when the publication cannot be trusted.** If
> `jit_active` and the pin set is empty, run the pause mark-only.

Built and measured (`CRATONVM_G1_PIN_EMPTY_PUBLICATION`, default OFF).
`G1Collector::empty_jit_publication` detects the state — a compiled frame is
live (`gc_quiescence::is_active()`, or the A5 unregistered-frame detector) and
both pin vocabularies are empty — and `collect_garbage` returns an empty
collection set. That is G1's only production entry to `young_collection` and
`mixed_collection` (the trait impl has no separate full-GC or compaction
method), so one refusal covers every path that could move the object.

It stops the corruption and it is still not a fix:

| arm | runs | outcome |
|---|---|---|
| pristine `684f37e14` | 6 | `IllegalFormatConversionException: d != java.lang.Object` |
| + refusal | 7 | **no format exception** — `Tests run: 14, Failures: 0, Errors: 8`, every error `OutOfMemoryError: Java heap space` |
| + refusal, lever off (`same binary`) | 2 | `IllegalFormatConversionException` returns |

ABBA-interleaved, 3 rounds, on azure host 2. The third row is the control that
matters: the same binary with the lever flipped reproduces the original failure,
so the refusal — not the rebuild — is what changed the outcome.

**Why it fails: the empty publication is a standing property of that frame, not
a sample.** A refused pause reclaims nothing, so the next allocation re-triggers
a pause that is still inside the same compiled frame and still publishes
nothing. Under `CRATONVM_G1_DBG_PINS` the fix arm shows **1444 consecutive**

```text
[g1][PINS] pause REFUSED: jit_active=true unregistered_frame=false pin_addrs=0 tlab_skips=0 -> empty CSet
```

and then dies of heap exhaustion. This is precisely the failure mode
`CRATONVM_G1_COVERAGE_PIN` was measured to have (330264 no-op pauses for a run
needing one collection), reached from a completely different predicate. A
refusal can only buy time for a publication that later becomes non-empty; this
one never does.

So candidate 1 trades a wrong answer on 1 test for an `OutOfMemoryError` on 8.
It ships OFF.

## What the lever IS good for: it discriminates, and `COVERAGE_PIN` does not

The predicate is narrow in exactly the way the coverage one is not. Measured
with the base binary and `CRATONVM_G1_DBG_PINS=1`, counting only pauses taken
with a compiled frame live (`young_collection`'s serial arm — the parallel arm
is gated on `!is_active()`, so an in-JIT pause is always the instrumented one):

| workload | in-JIT pauses | of those, `pin_addrs=0` |
|---|---|---|
| `MovingYoungConcurrentProbe 6 400 2000`, `--Xmx 256m` | 1 | **0** (`pin_addrs=18`) |
| `MovingYoungConcurrentProbe 6 400 2000`, `--Xmx 128m` | 2 | **0** (`pin_addrs=18`) |
| `PolynomialTest`, `--Xmx 1g` | 1 | **1** |

On the same probe, `moving_young_coverage_incomplete()` — what `COVERAGE_PIN`
gates on — is true for 330263 of 330264 pauses. So the coverage lever cannot
tell these two workloads apart at all, while this predicate separates them
cleanly: the conservative scan is *working* on the probe (18 addresses
published per pause) and publishing *nothing* on the failing test.

That is the useful result. It narrows candidate 2 from "the contract in
`conservative_roots`'s header is false" to "the contract holds for the compiled
frames on this probe and fails for this one", which is a far smaller thing to
go and find.

Detection is therefore counted on **every** run, not just under the lever:
`gc_metrics::record_g1_pause_empty_jit_publication`, surfaced as

```text
[GC] g1 jit publication: pauses=N empty_while_in_jit=K (y%)
```

next to the existing `[GC] g1 root coverage:` rate (`CRATONVM_GC_STATS=1`), plus
a throttled `tracing::warn!`. A non-zero count there is a pause that evacuated
against a root set it could not prove — worth knowing even on a run that is not
refusing. The two refusals also carry distinct decision reasons
(`g1-no-evacuation-empty-jit-publication` vs
`g1-no-evacuation-root-coverage-incomplete`) and distinct degrade bits, so the
rare one is never triaged as the normal one.

**What the predicate does not catch.** Both halves are process-wide.
`is_active()` is a striped global depth with no thread identity and the pin map
is summed across threads, so it actually asks "some thread is in JIT and *no*
thread published anything". A peer in JIT that published nothing is invisible
once any other thread published one address. Narrower than the hole it closes,
and not what this failure was (`pin_addrs=0` was the process-wide total) — but
another reason to prefer repairing the scan over widening the predicate.

## Candidate fixes, in increasing ambition

1. ~~**Refuse to evacuate when the publication cannot be trusted.**~~
   **Measured and closed off as a default** — see above. Kept as
   `CRATONVM_G1_PIN_EMPTY_PUBLICATION`, default OFF, because it discriminates
   where `CRATONVM_G1_COVERAGE_PIN` cannot.
2. **Make the conservative scan cover what it claims to.** Find why this frame's
   reference is not at an 8-byte aligned spill slot the scan walks. **Open, and
   now the only candidate that can actually fix this.** Newly scoped by the
   table above: the scan publishes 18 addresses per in-JIT pause on
   `MovingYoungConcurrentProbe` and zero on this one, so the question is what is
   different about *this* compiled frame, not whether the contract holds in
   general.
3. Precise oop maps for JIT frames under G1, which the module header already
   names as the eventual answer. **Open.**

## Repro of the measurement

```bash
# both arms, from the same base commit
/data/h2ki-build.sh      # -> /data/tgt-h2ki-{base,fix}/release/cratonvm
/data/h2ki-run.sh        # -> /data/h2ki-run/results.tsv  (ABBA + controls)
/data/h2ki-rate2.sh      # -> /data/h2ki-rate/summary2.txt (empty-publication rate)
```
