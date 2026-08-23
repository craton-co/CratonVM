# GPULlama3.java on CratonVM: the model load, and the FFM class-identity defect behind it

**Status: RESOLVED 2026-08-22.** Both defects this record opened on are
fixed. The reported "model-load livelock" was quadratic application-visible
work in `Collectors.toMap`, fixed on 2026-08-22 (`cc69a7d49`). The FFM
class-identity defect it left open — every synthetic `MemorySegment` carried
the **interface** as its runtime class, so the JDK's own `checkcast` to
`AbstractMemorySegmentImpl` could not succeed — is fixed here, together with
a second defect found immediately behind it (`MemoryLayout.withByteAlignment`
was a no-op) and two doors that disagreed with the ones beside them.

Superseded documents:

* `docs/known-issues/gpu/bug-gpullama3-model-load-and-ffm-segment-class-identity.md`
  (this record's open form),
* `bug-gpullama3-unsafe-getshort-model-load-livelock.md`, whose diagnosis was
  wrong in every particular — see §6.

**The application now runs its real inference loop and still produces no
output**, for a reason the open record could not see because it never got past
the first `matmul`: the Vector API kernel it spends every token in is orders of
magnitude off HotSpot. That is a throughput matter and not a correctness one —
every value matches the oracle bit for bit — and it has its own record,
`docs/known-issues/perf/vector-api-dispatch-depth-20260822.md`. Half of it is
since FIXED: CratonVM ran the JDK's generic lane-at-a-time Java fallback for
every Vector API operation, and `VectorSupport`'s intrinsic entry points are
now whole-vector Rust kernels covering 100% of that kernel's calls. **3.8x**,
and the gap that remains is the JDK's own dispatch layer above them. See §5.2.

Two residuals are **not** (fully) fixed and have been re-homed rather than
dropped:

* the Vector API throughput above, now `vector-api-dispatch-depth-20260822.md`,
  and
* `FileChannel.read` into a heap buffer at ~8.7x HotSpot, whose stated cause
  in the open record is **refuted** — `docs/known-issues/perf/
  filechannel-heap-read-glue-depth-20260822.md`. See §5.1.

---

## 1. What was wrong

### 1.1 `Collectors.toMap` was quadratic — FIXED 2026-08-22

The whole "livelock" was one line of application code:

```java
// org.beehive.gpullama3.tokenizer.Vocabulary
IntStream.range(0, vocabulary.length).boxed()
         .collect(Collectors.toMap(i -> vocabulary[i], i -> i))
```

CratonVM implements `Collectors.toMap` natively, and all three arities
detected duplicate keys by scanning **every key collected so far**, calling
into Java for each comparison. That is O(n²) Java re-entries. A Llama-3.2
tokenizer is 128,256 entries.

    n= 4000    2312 ms          n= 4000     55.6 ms
    n= 8000    9351 ms   ---->  n= 8000    106.3 ms
    n=16000   42608 ms          n=16000    220.4 ms
    n=128256  ~46 min (extrap)  n=128256   1842   ms

HotSpot does the same work in ~12 ms at every one of those sizes, because
its accumulator is `map.putIfAbsent(k, v)` — only keys in the same hash
bucket are ever compared. The fix keeps a `hash -> indices` side index and
does the same. Fixed in `native-collections`; `probes/StreamToMapProbe.java`
and `probes/StreamDecomposeProbe.java` are the measurement, and
`probes/CollectorsToMapSemantics.java` is the semantics fixture.

The same change fixed a correctness bug in passing: the 3-arg and 4-arg
merges compared keys with a function that **cannot invoke a user `equals`**,
so they silently dropped duplicates for any key class defining its own
equality.

### 1.2 It was never a livelock

The original record classified this as a livelock on the evidence of a flat
WorkingSet under live CPU burn. Both halves were misread:

* **A flat WorkingSet does not mean "no progress."** The work was building a
  `HashMap` from data already read; that is compute over resident memory. A
  later sampling showed WorkingSet climbing 771 MB → 1004 MB in ten seconds,
  which is simply a different phase of the same load.
* **Left alone, it finishes.** A 27-minute run completed model loading and
  reached inference. Nothing was spinning.

The generalisable lesson: *CPU-burning with flat RSS* is equally consistent
with a spin loop and with an O(n²) algorithm over resident data. The cheap
discriminator is a stack sample, not a memory counter.

---

## 2. The class-identity defect, and the fix

### 2.1 What the failure was

With loading fixed, the run reached inference in ~70 s and died on the first
`matmul`:

```
java.lang.ClassCastException: class java.lang.foreign.MemorySegment
  cannot be cast to class jdk.internal.foreign.AbstractMemorySegmentImpl
```

with `IntVector.intoMemorySegment0` as the top frame.

`native-builtins/src/panama.rs` allocated every synthetic `MemorySegment`
with the class name `java/lang/foreign/MemorySegment` — the **interface** —
and a private 6-slot layout `[ptr, size, arena, ro, alive, offset]`. Real
Java has no instance whose class is an interface, and the JDK's own FFM
consumers rely on that: they cast a segment down to its abstract base and
read its internals. `jdk.incubator.vector` does it on every segment entry
point:

```
ShortVector.fromMemorySegment0Template   pc 22: checkcast AbstractMemorySegmentImpl
IntVector.intoMemorySegment0Template     pc 23: checkcast AbstractMemorySegmentImpl
```

`AbstractVector.defaultReinterpret` reaches the second of those for *every*
`reinterpretAsInts()` — it round-trips the vector through a scratch
`MemorySegment.ofArray(new byte[n])` — which is why the failure landed three
frames away from anything the application wrote.

Nothing about this is Vector-API-specific. It is every JDK consumer that
touches the FFM internals.

### 2.2 It was NOT a GC defect, and NOT a JIT defect

The failure surfaced under a `gc::guard` "root COLLECTION gap" error, which
reads like one and is a red herring: the guard fires whenever an address
fails to decode as its expected class, not only when the object was
reclaimed. Four controls, all reproducing the identical single failure:

| control | result |
|---|---|
| baseline | exit 1, 64 s, 1 guard hit |
| `--Xmx 8g` | exit 1, 60 s, 1 guard hit — collector never got the chance |
| `--XX:UseGc G1` | exit 1, 71 s, 1 guard hit — a different root protocol |
| `--nojit` | exit 1, 126 s, 1 guard hit — no compiled frames at all |

and under `--nojit` the guard printed `collections_now=0`: **not one
collection had run** when the "reclaimed" address was reported. That is
conclusive. (The 2026-08-17 `multianewarray`/`ClassId(0)` bug produced the
same signature for the same reason; the discipline of running the big-heap
and second-collector arms before believing this guard is what separated them
both times.)

### 2.3 The fix, and the option that did not survive contact

Segments minted by CratonVM now carry a concrete class of its own,
`cratonvm/internal/foreign/MemorySegmentImpl`, whose relationship to the
`MemorySegment` interface and to `AbstractMemorySegmentImpl` is **declared**
in `typecheck.rs`'s `synthetic_implements` — the same shape, and for the same
reason, as `cratonvm/internal/SystemLogger`'s relationship to
`java.lang.System$Logger`. The 6/8-slot layout is unchanged.

The open record named three options and called the first — "give the
synthetic segments a concrete class that is assignable to
`AbstractMemorySegmentImpl`" — the honest one, noting it "requires auditing
which inherited JDK methods then become reachable". That audit was done, and
its answer is that inheriting is **worse**, for a reason the record could not
have known without running it:

`class_manager::fabricate_class` gives a fabricated class
`first_field_index: 0`. Naming `AbstractMemorySegmentImpl` as its superclass
(the shape `cratonvm/synthetic/Process` and `SSLSocketOutputStream` use)
therefore aliases the superclass's three fields onto slots 0/1/2 of
CratonVM's layout — `length` onto `ptr`, `readOnly` onto `size`, `scope` onto
`arena`. `resolve_field_index_in_hierarchy`, which `get_field_by_name` and
`resolve_field_index_by_class_id` both go through, would then start answering
those three names with the wrong values for three live readers in this tree
(`panama::heap_seg_field(_, "readOnly")`, `foreign_ffm::p67_receiver_session`'s
`"scope"` probe, and the `"base"` probe beside it). It would have converted
one loud `ClassCastException` into three silent wrong values.

Superclassing has a second cost the no-superclass form does not: every
inherited JDK method nobody shadows becomes a silent misread of CratonVM's
slots. With `java/lang/Object` as the superclass, an unregistered method
raises `NoSuchMethodError` — a refusal rather than a plausible number.

**A cast that succeeds is only half an answer.** The code on the far side of
it calls the abstract base's methods, and this carrier has no superclass to
inherit them from. `ScopedMemoryAccess.loadFromMemorySegment` opens with

```java
msp.sessionImpl()                       // then session.checkValidStateRaw()
VectorSupport.load(.., msp.unsafeGetBase(), msp.unsafeGetOffset() + offset, ..)
```

so those three are not optional extras. They are registered, along with
`maxAlignMask`, `checkAccess` / `checkBounds` / `checkReadOnly`,
`isAlignedForElement` (both overloads), the covariant `scope()`, and
`toString()` — everything on `AbstractMemorySegmentImpl` that reads `length`,
`readOnly` or `scope`, or is abstract on it.

Every registration made on the interface is made on the new class too, by a
**loop at each site** rather than a second list. Native dispatch is keyed on
the receiver's class, so a method registered under only one name is a
`NoSuchMethodError` waiting for its first caller, raised from JDK code three
frames from the registration that forgot it.
`panama::tests::the_craton_segment_class_mirrors_the_interface` fails if the
two registration sets ever differ.

---

## 3. The second defect, found immediately behind the first

With the cast fixed, the run moved on to:

```
IllegalArgumentException: Target offset 0 is incompatible with alignment
  constraint 4 for segment MemorySegment{ kind: heap, address: 0x0, byteSize: 16 }
```

All nine registrations of `MemoryLayout.withByteAlignment(long)` were
`p67_return_this` — **the receiver, unchanged**. The alignment is the only
thing the method exists to change, so the answer was wrong for every caller
that asked, and wrong quietly: the returned object is a perfectly good layout
of the original alignment.

`IntVector.<clinit>` builds its element layout as
`ValueLayout.JAVA_INT.withByteAlignment(1)`, and every Vector API segment
store and load goes through it. So a `byte[]`-backed segment — whose maximum
alignment is 1 — was being asked for a 4-byte-aligned write, and
`heap_segment_check_access` correctly refused.

**The refusal was right and the layout it was handed was wrong.** That is the
worst shape a stub can take, because the error names the innocent half: a
reader who trusted the message would have gone looking at the alignment
check, which had no defect in it.

`withByteAlignment` now returns a copy carrying the requested alignment, and
refuses a non-power-of-two the way the JDK does.

---

## 4. Two doors that disagreed with the ones beside them

**Reflection asked a narrower question than the bytecode.**
`Class.isInstance` and `Class.isAssignableFrom` never consulted
`typecheck::synthetic_implements`, so a segment that a `checkcast
jdk/internal/foreign/AbstractMemorySegmentImpl` had just admitted answered
`isInstance` **false** for the same class. Both now route through a new
`NativeContext::synthetic_implements_declared`, which is the door
`aastore_element_assignable` already uses to keep reflective array stores on
the `aastore` rule: one implementation, three callers.

**A pool bean that always reported zero.** The open record's second minor gap
was that `ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)`
returned no `"direct"` pool. It now returns the three pools HotSpot returns,
in HotSpot's order and with HotSpot's `ObjectName`s (measured on Temurin
25.0.3+9, not recalled). The bean the JDK's own code reaches by other routes
(`JavaNioAccess.getBufferPool`, `VM.getDirectBufferPool`) was worse than
absent: an instance stamped with the `BufferPoolMXBean` **interface** — the
same defect as §2, one API over — whose four accessors were stateless lambdas
returning 0. Its receiver is now a concrete `cratonvm/internal/BufferPool`
and its counters are the direct-memory allocator's own live ones.

That last one matters more than a missing bean would. `DirectBufferCacheProbe`
reads "direct buffers allocated per read" as a **difference of two
`getCount()` readings**, so a constant zero reports a perfect
temporary-buffer cache no matter what the VM is doing. See §5.

---

## 5. The two residuals, re-homed

### 5.1 `FileChannel.read` into a heap buffer, and its stated cause refuted

The open record's other minor gap: `FileChannel.read` into a heap buffer is
far slower than HotSpot, attributed to `sun.nio.ch.Util`'s per-thread
temporary-direct-buffer cache not working, on the evidence of a stack sample
in which "`DirectByteBuffer.<init>` and `CleanerImpl.run` between them took
12 of 16 samples".

**That attribution is refuted.** Two independent measurements, once the pool
bean of §4 could report a number that moves:

* `DirectBufferCacheProbe 5000` on CratonVM: `direct_count_before=1
  direct_count_after=1 direct_delta=0 per_read=0.0`. Five thousand
  heap-buffer reads allocated **no** direct buffers, from a non-zero
  baseline. The cache works.
* A `--nojit --stack-sample-ms 3` profile of `FileChannelHeapReadProbe 40000
  24` — **3519 samples**, against the original's 16 — contains no
  `DirectByteBuffer.<init>`, no `CleanerImpl.run` and no
  `Util.getTemporaryDirectBuffer` in the read loop at all.

What the 3519 samples do show is the cost spread evenly across ~20 methods of
JDK NIO glue, none above 11%: `IOUtil.read` (382), `FileChannelImpl.implRead`
(338), `FileDispatcherImpl.read` (334), `AbstractInterruptibleChannel.blockedOn`
(303), `HeapByteBuffer.put` (248), `IOUtil.readIntoNativeBuffer` (211), and
down a long tail. That is not a defect with a location; it is the depth of
the call chain, interpreted.

The gap re-measured on this build: 31.8 and 33.3 µs per read pair over two
runs, against HotSpot's 3.65 µs — **~8.7x**, not the 12x the original
recorded. Both numbers are `FileChannelHeapReadProbe 20000 24`.

This is a general NIO performance characteristic, not a GGUF or an FFM
defect, so retiring this record must not retire it. It is now
`docs/known-issues/perf/filechannel-heap-read-glue-depth-20260822.md`, with
the profile, the refuted hypothesis, and the shape of the fix that was
considered and not taken.


### 5.2 The Vector API is a Java fallback, and it is why there is still no token

This one the open record could not have written: it never got past the first
`matmul`, so the kernel behind it had never run. With the cast fixed it runs,
and the application's own headline claim — "it still does not produce output"
— is still true, for an entirely different reason.

`probes/Fp16VectorDotBench.java` prices `FP16FloatTensor.vectorDot` directly,
so the cost can be stated in nanoseconds rather than in "still running":

| VM | ns per lane |
|---|---|
| HotSpot | 0.268 |
| CratonVM | 115 561 – 139 180 (four runs, JIT on) |

and end to end, the same command the Repro section gives with `-n 1`:

| VM | result |
|---|---|
| HotSpot | 8.2 s wall, 2.51 s generating the token |
| CratonVM | 56 min at 97% of one core, no token, killed |

The two agree rather than contradicting: ~1.2e9 lane multiply-adds per
forward pass at ~116 µs each is tens of hours per token. **The application is
not stuck.** RSS is flat under continuous CPU and a profile puts it in the
kernel — which is the reading error §1.2 records this record's family making
once already, now with the answer in hand rather than guessed.

A 3500-sample `--nojit` profile puts 93.7% of the kernel in the Vector API's
own generic Java fallback: `bOpTemplate`, `uOpTemplate`, `lanewiseTemplate`,
`lambda$binaryOperations$13`, `vectorFactory`, `VectorSupport.maybeRebox`,
`VectorPayload.<init>` — a lambda per operation and a fresh vector object per
result, which is exactly what C2 exists to erase. `native-builtins/src/
vector_api.rs` implements a large Vector API surface already, but it
registers on `jdk/incubator/vector/FloatVector` and friends while a real-JDK
receiver is a concrete `Float256Vector`, so none of it is reached here.

**Correctness is not in question, and the probe says so out loud.** Every
value matches Temurin 25.0.3+9 exactly — `FfmVectorSegmentProbe` checks all
eight FP16 lanes and their sum, and `Fp16VectorDotBench` prints one `dot()`
call's raw float bits as a `warm=` column, identical on both VMs. A
throughput page that skipped that would leave a reader unsure whether the
slow answer was also a wrong one.

One lever inside it was implemented and then **reverted**, and the reversal is
the more useful record. `java.math` frames under `Math.fma` are 26.1% of that
profile — the JDK's `Math.fma` fallback builds two `BigDecimal`s and does a
`BigInteger` Knuth division per call, and `FloatVector.fma` calls it per lane.
Registering `fma` on `java/lang/Math` (CratonVM already has it on
`StrictMath`) changed the kernel by nothing distinguishable from noise,
because:

* with the JIT on — the mode that matters — `Math.fma` is already ~3 ns per
  call, the same as `a * b + c`; the 26.1% is an artefact of the `--nojit` the
  sampler requires, and
* in the interpreter the registration does not win anyway: `Math.fma` has real
  bytecode and is not on `force_native_over_real_jdk_bytecode`'s list.

An inert registration is exactly what this tree keeps having to un-ship, so it
was not shipped. `probes/MathFmaProbe.java` was kept, because it is a
correctness vector and it found something: 35 of 39 rows match HotSpot bit for
bit, and the four that do not are all `0 × Infinity`, where CratonVM answers
the canonical NaN and the oracle answers the x86 indefinite one — the known
`Value::Double` NaN-payload limitation, which predates this work.

Full measurement: `docs/known-issues/perf/
vector-api-dispatch-depth-20260822.md`.
**UPDATE 2026-08-22, later the same day.** The fallback half of this is fixed.
`native-builtins/src/vector_support_intrinsics.rs` implements the nine
`VectorSupport` entry points HotSpot marks `@IntrinsicCandidate` as
whole-vector Rust kernels, and on this kernel they cover **every call**
(`fell_back=0`). Measured with a kill switch on ONE binary, arms interleaved:
**79 713 / 80 479 ns per lane off, 20 813 / 21 276 on — 3.8x**, with the
`warm=` checksum identical on all four runs and on HotSpot.

It does not make the application produce a token, and the arithmetic in this
section still holds at the new rate. What it changes is the SHAPE of what is
left: the 93.7% that was `bOpTemplate` / `uOpTemplate` / `vectorFactory` and
the per-lane lambdas is gone, and the profile is now the JDK's own route TO
`VectorSupport` — `lanewiseTemplate`, `convert0`, `ImplCache.find`, `opCode`,
`sameSpecies` — several dozen ordinary Java calls per vector operation, which
at CratonVM's ~200 ns per call is the whole remaining cost. That is the general
call-cost story, not a Vector API defect, and it is the same shape as §5.1.

The rewritten record also corrects two claims this section made. Neither
`vector_api.rs`'s registrations nor the `Math.fma` one lose a dispatch race:
both live in registrars reachable only from `register_synthetic_overrides`,
which does not run in real-JDK mode, so neither had ever been registered there.
"The native did not win" and "the native was never registered in this mode"
look identical from the outside; the registrar's call path is the thing to
check first.


---

## 6. What the original record got wrong

Kept deliberately, because the errors are instructive rather than careless.

| claim | reality |
|---|---|
| "livelock … never completes" | completes; the load was O(n²) |
| "flat WorkingSet ⇒ no forward progress" | flat RSS is what compute over resident data looks like |
| "Suspect area: `Unsafe.getShort`" | `Unsafe.getShort` is not on the hot path at all |
| "`FloatTensor` FP16 reader spins" | the cost was in `Vocabulary`'s constructor and `GGUF.readArray` |
| suspicion drawn from a HotSpot startup warning | the warning names a deprecated API, not a hot method |
| "the temporary direct-buffer cache does not work" | it does; `direct_delta=0` over 5000 reads (§5.1) |
| "option 1 (a real superclass) is the honest one" | it aliases three field names onto the wrong slots (§2.3) |
| "it throws on the first `matmul`" (the only reason for no output) | it no longer throws, and still produces none: the kernel is ~500,000x (§5.2) |

Every one of the first five followed from reasoning about which code *looked*
suspicious. A single `--stack-sample-ms` run with `--nojit` — 797 samples,
about three minutes — named `Vocabulary.<init>` and `GGUF.readArray` directly
and would have replaced the whole hypothesis at the start.

The sixth is the same failure in a later costume, and it is the one worth
carrying forward: **the original stack sample had 16 samples.** Sixteen
samples of a JIT-on run, in which compiled frames are invisible to the
sampler, is not a profile — it is a handful of whatever happened to be
interpreted. The re-take used `--nojit` and got 3519, and the two disagree
completely about where the time goes.

---

## 7. Verification

`probes/FfmVectorSegmentProbe.java` is new: the Vector-API-over-segment path
in GPULlama3's own shape — an arena load, a heap store, the
`castShape`+`reinterpretAsInts` round trip that is
`AbstractVector.defaultReinterpret`, the whole `FP16FloatTensor.vectorDot`
kernel reduced to per-lane raw float bits plus a sum, and the
`withByteAlignment` behaviour of §3. Every line of its output matches Temurin
25.0.3+9 exactly, including all eight FP16 lanes and their sum.

The one line that cannot match is printed as `IDENTITY-NOTE` and says so:
CratonVM has no `NativeMemorySegmentImpl`, so the runtime class name differs.
What the probe asserts instead is the pair of answers that must match and now
do — `isInterface=false` and `abstractBase=true` — which is what every JDK
consumer actually depends on.

    RForeignLayoutCollections PASS
    RForeignLayoutJdkInterfaces PASS
    RJdkForeign PASS

Three more probes came out of the work and are in the tree:
`Fp16VectorDotBench` and `SegmentAllocBench` price §5.2's two costs, and
`MathFmaProbe` is the `Math.fma` correctness vector §5.2 describes.

`SegmentAllocBench` earned its place as an A/B **control** rather than a
benchmark. The first version of the class change resolved
`CRATON_SEGMENT_CLASS` by NAME on every allocation, through the shared
`try_alloc_concurrent_synthetic` — and that is a loader-faithful resolution
plus a `String` clone of the class name, not a map hit. It cost 15-40% on
every segment CratonVM mints, measured with the arms interleaved and one
binary built from the merge base:

```text
       ofArray_ns   arena_ns   asSlice_ns
base      7637        6482        6873
before    9849        7075        8321
base      7420        6087        7109
before   10679        8693        7756
```

Resolving the `ClassId` once per VM and allocating against it removed that and
then some — after, `arena` is 4380-4706 against the base's 5036-5690 and
`asSlice` 4866-4959 against 5555-6007, with `ofArray` on par. It is worth
noting how narrowly this was caught: it is invisible to every correctness
vector, and the workload that would have shown it is the one that cannot
finish. The control exists because `AbstractVector.defaultReinterpret` mints a
segment per `reinterpretAsInts()`, so a per-allocation regression here is
multiplied by every lane group in the kernel.

Core regression suite on the fixed binary: **66 of 67 vectors pass**. The one
failure, `RTreeRangeGc`, is pre-existing and was filed at the time as a
root-collection gap. **FIXED 2026-08-22**, and it was not a root-collection
gap: three defects in the collection natives, retired to
`rtreerangegc-was-four-collection-native-defects-FIXED-20260822.md`. The guard
text this paragraph leans on is emitted on EVERY failing `checkcast`, so it was
never evidence of reclamation -- which does not weaken the argument here, since
that argument rests on the SAME text appearing on both binaries rather than on
what the text means.
That is established rather than assumed: the same vector run 3x on this binary
and 3x on one built from the merge base fails on both, with identical guard
text — 3 of 3 here against 2 of 3 there, which is the flake's own rate rather
than a change in it.

## 8. Repro

```bash
cd apps/GPULlama3.java
cratonvm.exe --java-home <jdk25> --add-modules jdk.incubator.vector \
  -cp target/gpu-llama3-1.0.0-jdk25.jar org.beehive.gpullama3.LlamaApp \
  -m <models>/Llama-3.2-1B-Instruct-F16.gguf -p "hi" -n 5
```

To profile rather than guess, add `--nojit --stack-sample-ms 250` and
aggregate the deepest frame of each `T19.H1 stack dump`. `--nojit` matters:
JIT-compiled frames never reach the dispatch loop and so are invisible to the
sampler, which is why an earlier sampling of the same run returned 16 samples
of mostly-irrelevant frames — and, as §6 records, a wrong answer.
