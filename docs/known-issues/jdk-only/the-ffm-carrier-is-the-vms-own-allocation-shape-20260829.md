# The FFM carrier is the VM's own allocation shape — the contract question, decided

**Answers §3 of
[`arena-and-memorysegment-hand-out-an-interface-and-jdk-only-is-the-worse-mode-20260829.md`](arena-and-memorysegment-hand-out-an-interface-and-jdk-only-is-the-worse-mode-20260829.md),
which stated it so it could be decided once and deliberately did not decide it.**
2026-08-29.

> Is `cratonvm/internal/foreign/MemorySegmentImpl` a **compatibility stand-in**,
> or **the VM's own internal allocation shape**?

**It is the VM's own internal allocation shape.** Five independent signals, all
pointing the same way, and none of them a matter of taste.

---

## 1. The evidence

**1. It stands in for no JDK class, because there is no JDK class of that name.**
A compatibility stand-in exists so bytecode compiled against a JDK type can link
against ITS NAME. The JDK's segment implementations are
`jdk.internal.foreign.NativeMemorySegmentImpl`, `HeapMemorySegmentImpl` and
`MappedMemorySegmentImpl`. CratonVM's carrier claims to be none of them, and
nothing in any image is named `cratonvm/internal/foreign/MemorySegmentImpl`.

**2. Its JDK relationships come from the type table, not from its name.**
`typecheck.rs`'s `synthetic_implements` declares the carrier to be a
`java/lang/foreign/MemorySegment`, a `jdk/internal/foreign/AbstractMemorySegmentImpl`
and a `java/lang/foreign/SegmentAllocator`. Every `checkcast` the JDK's own code
emits — and `jdk.incubator.vector` emits one on **every** segment entry point —
is admitted by that table. **The name is doing no linking work.** Take the name
away and nothing that currently succeeds would stop.

**3. It deliberately does NOT have the JDK's layout, and the code says why.**
The same comment rejects giving the carrier a real superclass through
`fabricate_class` — the shape a compatibility stand-in uses — because a
fabricated class gets `first_field_index: 0`, so `AbstractMemorySegmentImpl`'s
`length`/`readOnly`/`scope` would alias slots 0/1/2 of CratonVM's carrier, which
hold `ptr`, `size` and `arena`. **A stand-in mimics the shape it stands in for.
This one has its own and was kept that way on purpose.**

**4. It is one of a family of three, all minted the same way for the same
reason.** `cratonvm/internal/SystemLogger` and `cratonvm/internal/BufferPool` sit
beside it in that table, with near-identical comments — *"the receiver used to be
stamped with the INTERFACE"*. Nobody would call a `BufferPoolMXBean` carrier a
compatibility stand-in for a JDK class; there is no JDK class it stands in for
either.

**5. The door's own doc names the category.**
`ClassManager::ensure_generated_class` is documented for *"the classes a
conforming JVM creates without any class file — array-adjacent shapes, lambda and
proxy implementation classes, reflection accessors, **and the VM's own internal
allocation shapes**. Those are legal in both modes (contract §1 item 6)."* The
carrier is the fourth item on that list, word for word.

### What would have made it a stand-in

Worth stating, so the decision is falsifiable rather than merely argued. Any ONE
of these would have settled it the other way:

* the carrier minted under a **JDK name** (`java/nio/Buffer$2` is minted that
  way, and that one IS a stand-in);
* bytecode anywhere referencing `cratonvm/internal/foreign/MemorySegmentImpl` by
  name;
* the carrier laid out to match `AbstractMemorySegmentImpl`'s fields so JDK
  bytecode could read them directly;
* a JDK class of the same name that CratonVM declines to load.

None of them holds.

---

## 2. What follows, and what it costs

Both halves follow from the answer, exactly as the question's page said they
would:

* mint the carrier through the generated-class door instead of the
  compatibility-stub door, so `--jdk-only` stops refusing it and landing on the
  interface;
* give the four `Arena` factories a carrier of their own instead of allocating
  with `java/lang/foreign/Arena`, the interface's own name.

**The cost, stated rather than buried: `compatibility_classes` stops counting
FFM programs.** The question's page flags that as the risk — *"Contract §11's
zero-stub census becomes unfalsifiable if a compatibility stand-in is minted
through this door"* — and it is the right warning for a stand-in. For an
allocation shape it is not a loss of signal but a **correction of it**: the
census counts compatibility substitutions, and counting the VM's own allocation
shape as one made every FFM-touching program report a substitution it was not
making. The number goes down because it was measuring the wrong thing, and the
way to keep that honest is to say so here rather than to let a quieter number
speak for itself.

**The decision covers the family, not one class.** All three carriers —
`MemorySegmentImpl`, `SystemLogger`, `BufferPool` — are minted through
`try_ensure_synthetic_class` today, so the same argument applies to all three and
there is no precedent among them to appeal to. This page decides the FFM one
because that is the one with a measured defect behind it; the other two are named
here so the next reader knows the argument already covers them.

---

## 3. What is NOT decided here

* **Whether the DoD screen should flag anything else.** If the answer had been
  "stand-in", the question's page says the compatible carrier should have been
  flagged too. It is not, on this answer, and no other census row changes.
* **The `Arena` interface stamp in compatible mode** is a separate, smaller
  defect: the factories allocate with the interface's own name, which is wrong in
  both modes independently of the door question.
* **Nothing about the nine behavioural defects**, which were fixed and measured
  in the companion record and are unaffected by which door mints the carrier.
