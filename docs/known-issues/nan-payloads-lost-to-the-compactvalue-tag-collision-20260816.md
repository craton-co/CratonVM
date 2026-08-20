# A quarter of NaN payloads are destroyed by the NaN-boxed value encoding

## Status
**OPEN, root-caused, now COUNTED — the encoding change is still not attempted
(updated 2026-08-17).** The mechanism was already understood and in the source.
What is new: an independent second sighting from a different census, a counter
so the loss is no longer silent, and an argument that narrows the fix list from
three options to one.

Originally found while censusing `java.lang.Math` against a HotSpot JDK 25
oracle for the `GaussNewtonOptimizerWith*Test` `hypot` bug (see the retired
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

Still 49 667 / 200 000 on 2026-08-17, unchanged.

**Independently reproduced from the other end.** `NanSurface.java`, written for
the `expected NaN but was NaN` comparison cluster, censuses 13 160 rows of
NaN-observable behaviour against a HotSpot oracle — wrappers, arithmetic, array
helpers, collections, comparators, sorts, streams. Once that cluster's own
defects were fixed, **397 of the 534 remaining rows are this bug**, and every one
of them involves a double whose pattern is `0xFFFC_…` or above. Two censuses
built for different questions, agreeing on the same 14 bits. See
bug-commonsmath-vector-nan-comparison-cluster-20260816-FIXED.

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

## NEW (2026-08-17): the loss is counted

The cheapest item on this write-up's own fix list is done.
`CompactValue::double`'s collision arm now calls
`compact_value::note_nan_payload_collapse()`, and
`compact_value::nan_payload_collapse_count()` reports the process total. The
first collapse in a process also prints one line to stderr, gated by a `Once`
exactly like the sibling `emit_first_degradation_diag`, and worded so a reader
who greps it does not go hunting for a memory-safety bug — this is a fidelity
event, not an unsafe one.

Cost: a relaxed `fetch_add` on an arm that was already taken. Every non-NaN
double and every positive NaN pays nothing.

The counter is deliberately **not** folded into `DEGRADATION_COUNTS`. A
degradation is a reference-shaped word that was refused (memory safety); this is
a double that lost its payload (fidelity). One number for both would make the
alarming case and the benign case indistinguishable, which is the mistake that
sink's own header warns about.

`the_nan_collapse_set_is_exactly_the_tag_pattern_and_is_counted` pins the set:
eight patterns that must round-trip verbatim (including a *negative* quiet NaN
whose marker bit is clear, and a *positive* payload NaN), four that must
collapse, and the counter reading exactly four afterwards. It is a
characterisation test — if the encoding is ever made lossless, that test is what
has to be rewritten, deliberately, rather than a count quietly going to zero.

## NEW (2026-08-17): only one of the three candidate fixes survives

The original list was "reserve a different tag pattern / store doubles
indirect / count it". Working through the first two:

* **Reserve a different tag pattern — impossible.** All 2^64 bit patterns are
  reachable doubles (`Double.longBitsToDouble` takes an arbitrary `long`), so no
  64-bit tag can be made unreachable by construction. The usual escape — store
  doubles offset, as JavaScriptCore does — needs the double image to be
  *smaller* than the slot, and here it is exactly the slot.
* **Carve a spill region out of an unused tag sub-region — also impossible, and
  this is the part worth recording**, because it looks available and is not. The
  tag space is 3 sub-tag bits x 47 payload bits, and several sub-regions are
  logically impossible for their own tag (`SUB_NULL`/`SUB_UNINIT` with a
  non-zero payload, `SUB_RETADDR` with payload bits above 31, `SUB_INT`/
  `SUB_FLOAT` with payload bits above 31). Every one of those is **already
  claimed**: the "BC SM2 fix 2026-05-28" made each decode as a *long*
  bit-pattern collision, which is what lets `CompactValue::long` store verbatim.
  Taking any of them back for doubles would re-break longs, where the loss is a
  wrong *value* rather than a wrong payload. And a colliding double needs 50
  bits of state where a sub-region offers 47.
* **Store doubles verbatim, exactly as longs already are.** Mechanically this
  now works — the descriptor-aware decoder already reinterprets raw bits for a
  `D` slot (`_ => Value::Double(f64::from_bits(self.0))`). It is **rejected on
  purpose**: a verbatim double would then be indistinguishable from a
  `SUB_OBJECT` slot to the *context-free* decoder and the GC root scanner, which
  is the same hazard this file's own HARD-AUDIT note documents for longs. Longs
  must take that risk because a long's value needs all 64 bits; doubles would be
  taking it to preserve a JLS-unspecified payload. Trading a possible mis-rooted
  GC reference for NaN-payload fidelity is the wrong direction.

**So the only remaining lossless option is to widen the slot** — a 16-byte
`Value`, or a side table reached through a sub-tag that would first have to be
freed elsewhere. That is a change to the hottest representation in the
interpreter, it should be justified by a workload that needs it, and the counter
added above is how that justification would be obtained. Nothing has produced
one yet.

## NEW (2026-08-17): the first workload reading is ZERO

`RealVectorTest` (82 tests), `SparseRealVectorTest` (106) and `StatUtilsTest`
(18) — the three commons-math classes that exercise NaN hardest, and the ones
the `expected NaN but was NaN` cluster was filed against — produce **no
collapse line at all**. Not "few": none. The encoding's lossy case was never
reached by any of them.

That is one data point, on the workload most likely to hit it, and it is the
first evidence in either direction. Widening the interpreter's hottest
representation to fix a case that a NaN-heavy numeric suite never reaches would
be a bad trade, and this number is why.

## Next step for whoever takes this

Extend the reading. Run the broader suites (Spring Boot, Netty, H2, Tomcat) and
grep stderr for `NaN payload collapsed`. If it stays absent everywhere, this is
a documented, measured, deliberate limitation and should be RETIRED as such
rather than fixed. If some workload lights it up, **that workload is the
argument for widening the slot**, and it should be named here — with the count,
so the cost of the fix can be weighed against something real.

`probes/F2dCensus.java` remains the fastest way to see it: self-contained, no
oracle needed.

## Repro

```bash
javac -d /tmp/classes probes/F2dCensus.java
<cratonvm-bin> --java-home <jdk25-home> --nojit -c /tmp/classes F2dCensus
java -cp /tmp/classes F2dCensus     # HotSpot: 0 wrong
```
