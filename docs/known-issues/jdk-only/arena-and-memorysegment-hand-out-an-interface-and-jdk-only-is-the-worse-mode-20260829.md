# The interface-classed FFM family: which door mints the carrier, and why `--jdk-only` refuses it

**Read
[`ffm-segment-surface-nine-behavioural-defects-and-the-interface-classed-family-20260829.md`](ffm-segment-surface-nine-behavioural-defects-and-the-interface-classed-family-20260829.md)
first — it is the record of this defect.** 199 rows, nine behavioural defects
fixed, and the identity residual sized across five class families rather than
the two this page reached.

This page is what it does not carry: **the mechanism, and the contract question
underneath it.** It was written the same day, independently, from the L1 lane,
and the two agree on every overlapping row — which is worth saying, because two
records of one defect are usually a contradiction waiting to be found.

---

## 1. Where the two agree

Measured here with `SegmentClassProbe` (59 rows, HotSpot 25.0.4+7, both modes):

| receiver | HotSpot | compatible | `--jdk-only` |
| --- | --- | --- | --- |
| all four `Arena` factories | concrete | **interface** | **interface** |
| every `MemorySegment` door | concrete | concrete | **interface** |

Same split the FFM record's §4 reports from 199 rows: compatible instantiates a
fabrication, strict refuses it and lands on the interface, and the families with
no fabricated carrier to fall back from are interface-classed in *both* modes.
Independent probes, same answer.

Everything else this probe asked passed in both modes — `byteSize`, int and long
round-trips, slice sizing, and five refusals including a read through a closed
arena and a double `close`. The carriers work; their advertised class is
impossible.

## 2. What this page adds: the mechanism is one line

`craton_segment_class_id` in `panama.rs` resolves the carrier like this:

```rust
match ctx.class_id_by_name(CRATON_SEGMENT_CLASS) {
    Some(id) => id,
    None => ctx.try_ensure_synthetic_class(CRATON_SEGMENT_CLASS, 0).ok()?,
}
```

`try_ensure_synthetic_class` is the door that mints **compatibility
stand-ins** — by its own doc, *"the one thing `--jdk-only` forbids"*. Strict
refuses, `.ok()?` yields `None`, and the caller falls back to stamping the
interface.

**So the fix for the interface stamp is itself a fabrication, and the mode whose
purpose is to refuse fabrications refuses it — landing back on the exact defect
the fabrication was introduced to cure.** Same shape as
`a-refused-syntheticstub-falls-through-to-an-older-native-not-to-bytecode`: a
refusal whose fallback is the older wrong answer rather than a refusal.

## 3. The contract question, stated so it can be decided once

There is a one-line change that makes the strict column match the compatible
one: mint through `ensure_generated_class`, which *"never refuses and never
records a violation"* and is documented for *"the VM's own internal allocation
shapes"*.

**Its doc also says, in bold, do not reach for it to silence a refusal** —
*"Contract §11's zero-stub census becomes unfalsifiable if a compatibility
stand-in is minted through this door: the substitution continues and the report
goes green."*

So the whole family reduces to one question:

> Is `cratonvm/internal/foreign/MemorySegmentImpl` a **compatibility stand-in**,
> or **the VM's own internal allocation shape**?

* **A stand-in.** It exists because this VM does not implement the JDK's FFM
  implementation classes, and the definition of done is *"no fabricated class
  instantiated, whatever its package"*. On this reading strict is **right** to
  refuse it, the compatible carrier is itself a DoD violation (which the FFM
  record's §4 confirms: any program touching FFM has
  `compatibility_classes > 0`), and the bug is that the refusal's fallback is an
  interface instead of a loud refusal.
* **An allocation shape.** It stands in for no JDK class and claims to be none;
  it carries zero declared fields and exists only so an instance has a concrete
  class. The alternative is not "more honest" — it is impossible in Java and
  breaks every JDK `checkcast`.

**Not decided here.** It is a contract question about the campaign's own
definition of done, it changes what the zero-stub census means, and the lane
brief's instruction for this shape is explicit: *"prefer recording a measured
finding over a speculative repair, and say which you did."*

**Also not done: fixing the `Arena` half alone.** All four factories
(`panama.rs:786/795/808/823`) allocate with `"java/lang/foreign/Arena"`, the
interface's own name, and giving them the carrier `MemorySegment` got would
improve compatible mode and no-op in strict — but it half-applies a change whose
strict behaviour is the open question.

## 4. For whoever takes it

Answer §3 once and both halves follow. If the answer is "allocation shape", the
change is the door swap plus an `Arena` carrier, and the check is this page's
table going all-concrete. If it is "stand-in", the change is at the fallback: a
refused carrier must refuse the OPERATION rather than hand back an
interface-stamped object, and the DoD screen should then flag the compatible
carrier too.

`SegmentClassProbe.java` is not in the tree — `probes/` was deleted wholesale on
2026-08-29 (`3b2901531`, 867 files). It is on the Linux build host at
`/data/l1u-probes/`, and the FFM record's own `FfmSegmentSweep` covers the same
ground more thoroughly from `apps/probes/`.
