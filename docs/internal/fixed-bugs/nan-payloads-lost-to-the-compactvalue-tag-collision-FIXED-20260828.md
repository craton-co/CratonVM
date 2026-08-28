# A quarter of NaN payloads were destroyed by the NaN-boxed value encoding

## Status

**FIXED, 2026-08-28.** `probes/F2dCensus.java` reports **0 / 200 000 wrong**,
where it reported 49 667 / 200 000 for twelve days. `probes/NanPayloadCensus.java`
— 351 rows across every route a NaN can take through the VM — is **byte-identical
to a HotSpot JDK 25 run, with the JIT on and with `--nojit`**.
`nan_payload_collapse_count()` is **0** on both censuses, on the 72-vector
regression suite, and on an H2 suite slice.

The encoding was not widened. Nothing about `CompactValue` grew past 8 bytes.

## What the previous revision of this page got wrong

It ended: *"the only remaining lossless option is to widen the slot … that is a
change to the hottest representation in the interpreter, it should be justified
by a workload that needs it."* That conclusion rested on a premise that was
false, and the premise was never checked:

> a verbatim double would be indistinguishable from a `SUB_OBJECT` slot **to the
> context-free decoder and the GC root scanner**

There is no context-free decoder on the paths that store doubles. Every slot the
interpreter puts a double into carries an out-of-band mark written by the *same
store*:

| store | mark |
| --- | --- |
| operand stack | `ValueStack::kinds[i] == KIND_DOUBLE` |
| local variables | `Frame::local_kinds[i] == LKIND_DOUBLE` |
| `RawSlot` bridge | the caller-supplied `SlotType::Double` |

Those marks were not added for this. They exist for the **long** side of exactly
the same ambiguity: `CompactValue::long` has stored bits verbatim since the BC
SM2 fix of 2026-05-28, because a long needs all 64 of them, and every consumer a
raw pattern could confuse was gated on the marks at that time:

* `ValueStack::scan_object_refs` and `update_object_refs` skip a `KIND_DOUBLE` /
  `KIND_LONG` slot before they look at the sub-tag — `KIND_DOUBLE` was already
  named in both.
* `Frame::scan_local_objects` and `update_local_refs` do the same for
  `LKIND_DOUBLE`; `frame.rs`'s own comment already said such a slot "is a
  primitive and is NEVER a GC root or a relocation target, regardless of bit
  pattern".
* `ValueStack::value_at`, `pop_double` and `push_with_kind_unchecked` already
  read and copied the raw bits when the mark said double — the last of those
  even documents that a `Value` round-trip "would re-encode a NaN-payload double
  through `CompactValue::double` and lose the payload".

So the machinery to make the encoding lossless for doubles was already built,
load-bearing, and tested. The one thing left was that the *encoder* threw the
payload away before any of it ran.

The `f2d` census also understated the blast radius. Fixing the encoder took it
from 49 667 to 18 622, not to zero — because three more readers were re-deriving
a double's type from the bits after the mark had already told them.

## The four things that were wrong

**1. The encoder.** `CompactValue::double` canonicalizes any pattern where
`bits & 0xFFFC_0000_0000_0000 == 0xFFFC_0000_0000_0000` — sign, exponent, quiet
bit, marker bit — i.e. every negative quiet NaN with mantissa bit 50 set. A
float NaN widens with its 23 mantissa bits shifted left by 29, so float mantissa
bit 22 lands on the quiet bit and bit 21 on the marker: negative float NaNs with
both set are destroyed and nothing else is, which is why the sweep read
`negative 1024/2048, positive 0/2048`.

Added `CompactValue::double_raw` (verbatim, contract: *the caller writes the
mark*) beside `CompactValue::double` (context-free, still canonicalizing, still
counting), and routed every marked store through it: both `ValueStack` double
pushes, the three `push_compact_double*` call sites, `push` / `push_unchecked` /
`push_checked` via the new `from_value_kinded`, `Frame::set_local*`,
`copy_args_to_locals`, and `RawSlot::to_compact(SlotType::Double)`.
`Frame::get_local` and `get_local_unchecked` grew the matching `LKIND_DOUBLE`
read arm — a no-op for every pattern that could be stored *before* the change,
since an untagged double already decoded as `Value::Double` with the same bits.

**2. `decode_by_descriptor(b'D')`.** Its `SUB_INT` arm widened the low 32 bits as
an int and its `SUB_NULL`/`SUB_UNINIT` arm answered `0.0`, for any slot whose
bits matched those sub-tags. After the encoder fix the census survivors were
exactly three of the eight sub-tags — `SUB_INT` (0), `SUB_NULL` (3),
`SUB_UNINIT` (4), which is 3/8 of the collision set and precisely the
`negative 384/2048` the by-sign sweep then printed. The other five already
reinterpreted the raw bits.

Guarded them the way the sibling `b'J'` arm already guarded the long side of the
identical collision: a real int slot has payload < 2^32, a real null or
uninitialized slot has payload 0, so a payload outside those ranges under a `D`
descriptor is a collided double and the bits are the answer. The genuine cases —
an int left where a double was expected, an unwritten slot reading as JVMS §2.3's
default `0.0d` — are untouched.

**3. The invoke-argument mark was a boolean that could only say LONG.** The
marshalling popped `(CompactValue, is_long)`. A `D` parameter's `KIND_DOUBLE`
slot arrived as `is_long = false` and fell through to `decode_by_descriptor`,
which read whichever sub-tag the bits matched. `0xFFFC_0000_0000_0001` is
bit-for-bit `CompactValue::int(1)`, so `Double.doubleToRawLongBits` was handed
`1.0`. The kind byte is now carried instead of the boolean — `pop_with_kind`
already existed — through all seven `decode_arg_kind_aware` call sites, the JIT
bridge's saved-argument array, and `pop_arg_for_descriptor_checked`. The two
JIT-deopt restores, which re-pushed saved args with `push_compact` and therefore
demoted a `KIND_DOUBLE` slot to `KIND_UNKNOWN`, now restore bits **and** mark.
`pop_compact_with_long_mark` and its unchecked sibling are gone: leaving a
long-only pop beside a long-and-double one is how this defect got written twice.

**4. Three `D`-descriptor stores popped raw and re-derived the type.**
putstatic-D (`pop_static_field_value`), putfield-D (the `opcodes.rs` arm) and the
`dastore` fast path each ignored the mark and switched on `cv.tag()`. Each now
consults the mark first. This is what the `array` / `static-field` /
`instance-field` / `shuffle` rows of the multi-route census were failing on.

## Evidence

```
                                  before      after
probes/F2dCensus.java             49667/200000   0/200000     (--nojit)
probes/NanPayloadCensus.java      45 rows differ from HotSpot / 351
                                                0 rows differ  (--nojit and JIT)
nan_payload_collapse_count()      >0             0
```

`NanPayloadCensus` is new and is the artifact this page was missing: it walks
`f2d`, `d2f`, `d2f2d`, a local, a 1-argument and a 6-argument call plus returns,
`dmul`/`dadd`/`dsub`/`ddiv`/`dneg`, a `double[]` element, a static field, an
instance field, boxing, the stack-shuffle opcodes, `Double.doubleToLongBits`
(which must still canonicalize), `Double.isNaN`, the float-side routes, and
`Math.scalb(float, int)` at five exponents — for 14 double patterns straddling
the collision set and 9 float patterns, with controls that were never affected
(positive NaNs, a negative NaN with the marker bit clear, the infinities). Every
row is `route pattern -> hex`, so a diff against a HotSpot run names both.

`Math.scalb(float, int)` was the one place the loss had already cost something
concrete — the JDK implements it as `(float)((double) f * 2^k)`, so the
intermediate double passed through a `CompactValue` slot and 8 of 6000 census
rows disagreed with HotSpot. Those rows are in `NanPayloadCensus` and they match.

Regressions:

* `regression-suite/run.sh`: **72 passed, 0 failed** (it compares against a real
  HotSpot run, so it is an oracle diff, not a self-check).
* `cargo test -p cratonvm-types --lib`: **589 passed, 0 failed**.
* `cargo test -p cratonvm-gc --lib`: **1687 passed, 0 failed**.
* `cargo test -p cratonvm-vm --lib`: 2636 passed, 2 failed —
  `runtime::resolve::guard::the_allowlist_has_no_dead_rows` and
  `no_unallowlisted_metadata_table_bypass_exists`. **Both are red on `dev`
  independently of this change**: the first offender they name is a stale
  allowlist row for `vm/src/native/jni.rs`, a file this branch does not touch and
  which is byte-identical to `origin/dev`.
* H2 suite, first 40 classes, JIT on: matches the tracked `baseline.tsv` on every
  class, with `TestIndex` passing where the baseline records FAIL.

## What is deliberately still lossy, and why it cannot be reached

`CompactValue::double` keeps canonicalizing, and the characterisation test that
pins its collapse set is unchanged. That is correct: a caller with no mark to
offer genuinely cannot be given one, and a slot with no mark genuinely cannot be
decoded. The counter therefore stays, and changes role — **it is now a ratchet.**
A non-zero reading means a *new* context-free double encode has appeared in the
tree, and the fix for that is to give the call site a mark, not to accept the
loss.

There is one narrower residue worth naming rather than leaving as a hole. A
*descriptor-only* decode — `decode_by_descriptor(b'D')` with no kind mark to
consult — still cannot recover two patterns:

* `0xFFFC_0000_0000_0000`, which **is** `CompactValue::int(0)`, and
* `0xFFFC_0000_0000_0001`, which **is** `CompactValue::int(1)`.

Bit-for-bit, not "resembles". `types/src/compact_value.rs`'s
`double_raw_keeps_every_tag_colliding_payload_and_counts_nothing` asserts that
identity directly so the exception is a statement about the encoding rather than
a gap in a test. Every slot the interpreter stores a double in carries the mark,
which is why both patterns round-trip correctly in `NanPayloadCensus` — including
through `Double.longBitsToDouble`, a native call, and a six-argument frame.

## Reproducing

```bash
javac -d /tmp/classes probes/F2dCensus.java probes/NanPayloadCensus.java
<cratonvm-bin> --java-home <jdk25-home> --nojit -c /tmp/classes F2dCensus
java -cp /tmp/classes NanPayloadCensus > /tmp/hotspot.txt
<cratonvm-bin> --java-home <jdk25-home> -c /tmp/classes NanPayloadCensus > /tmp/craton.txt
diff /tmp/hotspot.txt /tmp/craton.txt
```

## Provenance

Found while censusing `java.lang.Math` against a HotSpot JDK 25 oracle for the
`GaussNewtonOptimizerWith*Test` `hypot` bug (see the retired
`bug-commonsmath-gaussnewton-testmaxevaluations-no-exception-20260816`
write-up); it is not that bug and was not caused by its fix. Independently
re-found from the other end by `NanSurface.java`, written for the
`expected NaN but was NaN` comparison cluster: once that cluster's own defects
were fixed, 397 of its 534 remaining rows were this, and every one involved a
double whose pattern was `0xFFFC_…` or above. Two censuses built for different
questions agreeing on the same 14 bits.

The counter, the one-shot stderr line, and the characterisation test came from
the 2026-08-17 revision of this page and are all still here. What that revision
could not get — a workload reading that would justify the fix — turned out not to
be needed, because the fix was not the one it had costed.
