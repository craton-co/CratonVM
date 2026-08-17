# A quarter of NaN payloads are destroyed by the NaN-boxed value encoding

## Status
**OPEN, root-caused, not fixed (2026-08-16).** The mechanism is understood and
is in the source; the fix is an encoding change and is deliberately not attempted
here. Found while censusing `java.lang.Math` against a HotSpot JDK 25 oracle for
the `GaussNewtonOptimizerWith*Test` `hypot` bug (see the retired
`bug-commonsmath-gaussnewton-testmaxevaluations-no-exception-20260816`
write-up); it is not that bug and was not caused by its fix.

## The observable

`probes/F2dCensus.java` widens random NaN float patterns and compares against
the widening the IEEE 754 formats define — no oracle needed, though HotSpot
agrees with the formats on all 200 000:

```
HotSpot    f2d NaN census: 0 / 200000 wrong
CratonVM   f2d NaN census: 49667 / 200000 wrong
   in=ffe2a0d4 got=7ff8000000000000 want=fffc541a80000000
   in=ffb2f074 got=7ff8000000000000 want=fffe5e0e80000000
by sign: positive 0/2048, negative 1024/2048
```

Identical with `--nojit` and with the JIT enabled: this is not a dispatch-route
difference. The lost payload becomes visible through `Double.doubleToRawLongBits`
and through anything that carries a NaN sentinel's bits.

## Root cause

`types/src/compact_value.rs`. `CompactValue` is a NaN-boxed 64-bit slot: doubles
are stored raw, and every other kind is a quiet NaN with a sub-tag in the high
mantissa bits.

```rust
const NANBOX_BITS: u64 = 0xFFFC_0000_0000_0000;   // sign + exponent + quiet + marker
fn is_nan_tagged(v: u64) -> bool { (v & NANBOX_BITS) == NANBOX_BITS }

pub fn double(v: f64) -> Self {
    let bits = v.to_bits();
    if is_nan_tagged(bits) { Self(CANONICAL_NAN) } else { Self(bits) }
}
```

So a double is canonicalized exactly when it is **negative, quiet, and has
mantissa bit 50 set** — otherwise it would be indistinguishable from a tagged
float/object/long slot. That is a real constraint of the encoding, not an
oversight, and the collision check is the right defensive move given it.

It explains the measured signature exactly. A float NaN widens with its 23
mantissa bits shifted left by 29, so float mantissa bit 22 lands on double bit
51 (quiet) and float mantissa bit 21 lands on double bit 50 (marker). Hence:
negative float NaNs with mantissa bits 22 and 21 both set are destroyed, and
nothing else is — which is why the sweep reports `negative 1024/2048` and
`positive 0/2048`.

## What is wrong is the justification, not the code

The comment on `double()` said the canonicalization is harmless because "Java
mandates a single NaN anyway". Java does not. `Double.doubleToRawLongBits`
exists precisely so a program can observe the payload it was given, HotSpot
preserves it through `f2d`, `d2f`, `dmul`, `dadd`, array stores and field
stores, and this VM matches HotSpot on all of those *except* where the tag
collides. The comment has been corrected in place to say what is actually true:
the loss is a deliberate cost of the encoding, it is observable, and here is how
much of the space it covers.

## Blast radius

Payload-only. Every NaN still tests as NaN, still compares as NaN, and still
prints as `NaN`; `Double.isNaN`, `Double.compare`, `Double.equals` and
`doubleToLongBits` (the non-raw one, which canonicalizes by specification) are
all unaffected. Nothing in the commons-math suite depends on it. It is
observable through `doubleToRawLongBits`/`floatToRawIntBits`, and through any
protocol that ships a NaN's bits verbatim.

The one place it has already cost something concrete: it is why
`Math.scalb(float, int)` disagreed with HotSpot on 8 of 6000 census rows, since
the JDK implements it as `(float)((double) f * 2^k)` and the intermediate double
passes through a `CompactValue` slot.

## Fixing it

Not attempted. Every option is a change to the core operand representation:

* Reserve a different tag pattern — one that no *canonical-widening* result can
  produce. The current pattern is reachable by ordinary arithmetic, which is the
  whole problem.
* Store doubles boxed/indirect, and pay a slot indirection on the hottest
  numeric path in the interpreter.
* Keep the collision but record it, so the loss is at least observable rather
  than silent (the cheapest option, and a good first step: a counter here would
  say whether any real workload ever hits it).

Whoever takes this should start with `probes/F2dCensus.java`, which is
self-contained and needs no oracle.

## Repro

```bash
javac -d /tmp/classes probes/F2dCensus.java
<cratonvm-bin> --java-home <jdk25-home> --nojit -c /tmp/classes F2dCensus
java -cp /tmp/classes F2dCensus     # HotSpot: 0 wrong
```
