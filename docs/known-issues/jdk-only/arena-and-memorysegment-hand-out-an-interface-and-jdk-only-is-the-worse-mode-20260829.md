# `Arena` and `MemorySegment` hand out an INTERFACE as an instance's class — and `--jdk-only` is the worse mode

**Status: OPEN, fully diagnosed, deliberately NOT fixed.** 2026-08-29. The
blocker is a contract decision, not a missing patch, and §4 says exactly what
the decision is.

`HANDOFF-20260828-SCOPE.md` §4 carries this as a one-line row: *"`Arena`/
`MemorySegment` report an INTERFACE as an instance's class — `panama.rs`,
unclaimed, closest to L1"*. This page is that row measured.

---

## 1. What was measured

`SegmentClassProbe` — 59 rows, HotSpot 25.0.4+7 as oracle, both CratonVM modes.
It asks only what a caller can rely on (`isInterface`, `isAbstract`,
`instanceof`), never the class NAME, because a name is an implementation token
the two VMs may legally disagree on.

| receiver | HotSpot | CratonVM compatible | CratonVM `--jdk-only` |
| --- | --- | --- | --- |
| `Arena.ofConfined()` | concrete | **INTERFACE** | **INTERFACE** |
| `Arena.ofShared()` | concrete | **INTERFACE** | **INTERFACE** |
| `Arena.ofAuto()` | concrete | **INTERFACE** | **INTERFACE** |
| `Arena.global()` | concrete | **INTERFACE** | **INTERFACE** |
| `MemorySegment.ofArray(byte[] / int[] / long[])` | concrete | concrete | **INTERFACE** |
| `arena.allocate(n)` | concrete | concrete | **INTERFACE** |
| `segment.asSlice(..)` | concrete | concrete | **INTERFACE** |
| `segment.reinterpret(..)` | concrete | concrete | **INTERFACE** |
| `MemorySegment.NULL` | concrete | concrete | **INTERFACE** |

Every one of these also reports `isAbstract = true`, because an interface is.

**Real Java cannot produce an instance whose class is an interface**, and the
JDK's own FFM consumers depend on that. `jdk.incubator.vector`'s
`fromMemorySegment0Template` / `intoMemorySegment0Template` open with
`checkcast jdk/internal/foreign/AbstractMemorySegmentImpl`, which no interface
stamp can satisfy — the failure `panama.rs`'s own doc block records GPULlama3's
first `matmul` dying on.

**Everything else in the probe passes**, in both modes: `byteSize`, the int and
long round-trips through an allocated segment, slice sizing, and the five
refusals (slice past the end, negative slice offset, read past the end, read
through a closed arena, double `close`). The carriers work. It is their
advertised class that is impossible.

---

## 2. Two halves, and only one of them was ever fixed

**The `MemorySegment` half was fixed on 2026-08-22** — `panama.rs` introduced
`CRATON_SEGMENT_CLASS = "cratonvm/internal/foreign/MemorySegmentImpl"`, a
concrete carrier with zero declared fields, and its doc block records the two
alternatives that were rejected with measured reasons (reusing
`NativeMemorySegmentImpl` puts `ptr` where `length` is read; a fabricated
subclass of `AbstractMemorySegmentImpl` gets `first_field_index: 0` and aliases
the superclass's three fields onto slots 0/1/2).

**The `Arena` half was not.** All four factories still allocate with the
interface's own name:

```rust
r.register(arena, "ofAuto", "()Ljava/lang/foreign/Arena;", |ctx, _| {
    let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4)?;
```

`panama.rs:786, 795, 808, 823` — `global`, `ofAuto`, `ofConfined`, `ofShared`.

So the scope doc's row is right about `Arena` and stale about `MemorySegment`
in compatible mode — and, as §3 shows, right about `MemorySegment` again under
`--jdk-only`.

---

## 3. `--jdk-only` reverts the fix, and the mechanism is one line

This is the interesting half, and it is the reverse of this campaign's standing
finding that strict mode is the more correct one.

`craton_segment_class_id` resolves the carrier like this:

```rust
match ctx.class_id_by_name(CRATON_SEGMENT_CLASS) {
    Some(id) => id,
    None => ctx.try_ensure_synthetic_class(CRATON_SEGMENT_CLASS, 0).ok()?,
}
```

`try_ensure_synthetic_class` is the door that mints **compatibility
stand-ins** — by its own doc, *"the one thing `--jdk-only` forbids"*. Under
strict mode it refuses, `.ok()?` returns `None`, and the caller falls back to
stamping the interface.

**So the fix for the interface stamp is itself a fabrication, and the mode whose
whole purpose is to refuse fabrications refuses it — landing back on the exact
defect the fabrication was introduced to cure.** That is the same shape as
`a-refused-syntheticstub-falls-through-to-an-older-native-not-to-bytecode`:
a refusal whose fallback is the older wrong answer rather than a refusal.

---

## 4. Why this is not fixed here — the decision, stated

There is a one-line change that makes the strict column match the compatible
one: mint the carrier through `ensure_generated_class` instead. That door is for
*"the classes a conforming JVM creates without any class file — array-adjacent
shapes, lambda and proxy implementation classes and their superclasses,
reflection accessors, and the VM's own internal allocation shapes"*, and it
*"never refuses and never records a violation"*.

**Its doc also says, in bold, do not reach for it to silence a refusal** —
*"Contract §11's zero-stub census becomes unfalsifiable if a compatibility
stand-in is minted through this door: the substitution continues and the report
goes green."*

That is precisely what this change would be, unless the answer to one question
is yes:

> **Is `cratonvm/internal/foreign/MemorySegmentImpl` a compatibility stand-in,
> or the VM's own internal allocation shape?**

Both readings are defensible and they give opposite instructions:

* **A stand-in.** It exists because CratonVM does not implement the JDK's FFM
  implementation classes. The definition of done is *"no fabricated class
  instantiated, whatever its package"* — and `cratonvm/internal/...` is
  fabricated by that definition. On this reading strict mode is **right** to
  refuse it, the compatible mode's carrier is itself a DoD violation, and the
  bug is that the refusal's fallback is an interface instead of a loud refusal
  of the operation.
* **An allocation shape.** It stands in for no JDK class and claims to be none.
  It carries zero declared fields and exists so that an instance has a concrete
  class at all. The alternative it replaced is not "more honest" — it is
  impossible in Java and breaks every JDK `checkcast`.

**I did not take that decision.** It is a contract question about the
campaign's own definition of done, it changes what the zero-stub census means,
and the lane brief's instruction for this shape is explicit: *"a defect you
find here may not be safe to fix in isolation. Prefer recording a measured
finding over a speculative repair, and say which you did."* This is a record.

**What I also did not do: fix the `Arena` half alone.** Giving `Arena` the same
carrier the segment has would be a strict improvement in compatible mode and a
no-op in strict — but it half-applies a change whose strict behaviour is the
open question, and a half-applied change is worse than either endpoint.

---

## 5. For whoever takes it

* The two halves are **one defect with one blocker**. Answer §4's question once
  and both halves follow; `Arena` needs the same carrier treatment
  `MemorySegment` got, at `panama.rs:786/795/808/823`.
* If the answer is "allocation shape", the change is the door swap plus an
  `Arena` carrier, and the check is this probe's table going all-`false`.
* If the answer is "stand-in", the change is at the fallback: a refused carrier
  must refuse the OPERATION, not hand back an interface-stamped object — and
  the DoD screen should then flag the compatible-mode carrier too.
* The probe is not in the tree: `probes/` was deleted wholesale on 2026-08-29
  (`3b2901531`, 867 files). `SegmentClassProbe.java` lives on the Linux build
  host at `/data/l1u-probes/`, and is quoted in full in this page's history.

## Reproduce

```bash
javac -d out SegmentClassProbe.java
java -cp out SegmentClassProbe                                  # oracle
cratonvm --java-home "$JDK" -cp out SegmentClassProbe            # compatible
cratonvm --java-home "$JDK" --jdk-only -cp out SegmentClassProbe # strict
```

59 rows each. Diff the strict arm against the compatible one — the mode drift is
the finding, and it is 68 lines.
