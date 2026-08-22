# GPULlama3.java on CratonVM: model load fixed, an FFM class-identity defect still open

## Status
**PARTIALLY FIXED** (2026-08-22). The reported model-load livelock is fixed
and was not a livelock. A second, separate defect on the inference path
remains **OPEN** — it is an FFM/Panama class-identity defect, not a GPU or
Vector API one, and not the GC defect its error message suggests.

Supersedes `bug-gpullama3-unsafe-getshort-model-load-livelock.md`, whose
diagnosis was wrong in every particular — see "What the original record got
wrong" below. That is worth reading before trusting any similar triage.

## Severity
**MEDIUM** (was HIGH). The app now loads a real 2.4GB GGUF model and reaches
inference in ~70 seconds where it previously ran 27 minutes. It still does
not produce output: it throws on the first `matmul`.

## What was actually wrong, and what is fixed

### 1. `Collectors.toMap` was quadratic — FIXED

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

### 2. It was never a livelock

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

## Still open: every synthetic MemorySegment carries the INTERFACE as its class

With loading fixed, the run reaches inference in ~70 s and dies on the
first `matmul`:

```
java.lang.ClassCastException: class java.lang.foreign.MemorySegment
  cannot be cast to class jdk.internal.foreign.AbstractMemorySegmentImpl

at FloatTensor.matmul(FloatTensor.java:99)
at Parallel.parallelFor(Parallel.java:10)
at FP16FloatTensor.vectorDot(FP16FloatTensor.java:70)   <-- ShortVector.fromMemorySegment
at jdk/incubator/vector/... (Vector API)
```

**Root cause.** `native-builtins/src/panama.rs` allocates every synthetic
`MemorySegment` with the class name `java/lang/foreign/MemorySegment` —
the **interface** — and a private 6-slot layout
(`[ptr, size, arena, ro, alive, offset]`). 21 call sites do this. Any JDK
code that casts a segment to its abstract base therefore throws, and the
JDK does that routinely: the Vector API's `fromMemorySegment0` /
`intoMemorySegment0` both open with
`(AbstractMemorySegmentImpl) segment`.

So this is not specific to the Vector API or to this app. It is every FFM
consumer that touches the JDK's own segment internals. The same shape is
already worked around once, for a different consumer: see the "Wave 2 D —
DirectByteBuffer / Cleaner checkcast guard" note in `native-builtins`,
which shims `Buffer.session()` to dodge the identical
`checkcast AbstractMemorySegmentImpl`.

**It is NOT a GC defect, and NOT a JIT defect.** The failure surfaces
under a `gc::guard` "root COLLECTION gap" error, which reads like one and
is a red herring — the guard fires whenever an address fails to decode as
its expected class, not only when the object was reclaimed. Four controls,
all reproducing the identical single failure:

| control | result |
|---|---|
| baseline | exit 1, 64 s, 1 guard hit |
| `--Xmx 8g` | exit 1, 60 s, 1 guard hit — collector never got the chance |
| `--XX:UseGc G1` | exit 1, 71 s, 1 guard hit — a different root protocol |
| `--nojit` | exit 1, 126 s, 1 guard hit — no compiled frames at all |

and under `--nojit` the guard prints `collections_now=0`: **not one
collection had run** when the "reclaimed" address was reported. That is
conclusive. (The 2026-08-17 `multianewarray`/`ClassId(0)` bug produced the
same signature for the same reason; the discipline of running the
big-heap and second-collector arms before believing this guard is what
separated them both times.)

**Fix options**, none of them one-line, which is why this is filed rather
than fixed here:

1. Give the synthetic segments a concrete class that is assignable to
   `AbstractMemorySegmentImpl` while keeping CratonVM's 6-slot layout.
   Requires auditing which inherited JDK methods then become reachable on
   these objects — they would read CratonVM's slots under the JDK's field
   meanings, which is the trap that makes this more than a rename.
2. Special-case assignability so the synthetic segment class satisfies
   `checkcast`/`instanceof` against `AbstractMemorySegmentImpl`. Narrower,
   but it makes the type system lie in one more place.
3. Shim the Vector API's segment entry points the way `Buffer.session()`
   is shimmed. Cheapest, and whack-a-mole: the FFM surface is large.

Option 1 is the honest one. It should be scoped as Panama work, not as a
GPULlama3 fix.

## Other gaps this app surfaced (both open, both minor)

* **`FileChannel.read` into a heap buffer is ~12x HotSpot.** 94 µs/string vs
  7.4 µs on GGUF's read shape (`probes/GgufStringReadProbe.java`,
  `probes/FileChannelHeapReadProbe.java`). Worth ~11 s of the load. Not the
  livelock, and not chased further.
* **No `direct` `BufferPoolMXBean`.** `ManagementFactory.getPlatformMXBeans(
  BufferPoolMXBean.class)` returns no "direct" pool, so direct-buffer
  allocation is not observable from Java.
  `probes/DirectBufferCacheProbe.java` throws on CratonVM and runs on
  HotSpot.

## What the original record got wrong

Kept deliberately, because the errors are instructive rather than careless.

| claim | reality |
|---|---|
| "livelock … never completes" | completes; the load was O(n²) |
| "flat WorkingSet ⇒ no forward progress" | flat RSS is what compute over resident data looks like |
| "Suspect area: `Unsafe.getShort`" | `Unsafe.getShort` is not on the hot path at all |
| "`FloatTensor` FP16 reader spins" | the cost was in `Vocabulary`'s constructor and `GGUF.readArray` |
| suspicion drawn from a HotSpot startup warning | the warning names a deprecated API, not a hot method |

Every one of those followed from reasoning about which code *looked*
suspicious. A single `--stack-sample-ms` run with `--nojit` — 797 samples,
about three minutes — named `Vocabulary.<init>` and `GGUF.readArray` directly
and would have replaced the whole hypothesis at the start.

## Repro

```bash
cd apps/GPULlama3.java
cratonvm.exe --java-home <jdk25> --add-modules jdk.incubator.vector \
  -cp target/gpu-llama3-1.0.0-jdk25.jar org.beehive.gpullama3.LlamaApp \
  -m <models>/Llama-3.2-1B-Instruct-F16.gguf -p "hi" -n 5
```

To profile rather than guess, add `--nojit --stack-sample-ms 250` and
aggregate the deepest frame of each `T19.H1 stack dump`. `--nojit` matters:
JIT-compiled frames never reach the dispatch loop and so are invisible to
the sampler, which is why an earlier sampling of the same run returned 16
samples of mostly-irrelevant frames.
