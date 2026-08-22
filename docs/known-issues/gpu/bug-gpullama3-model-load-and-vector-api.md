# GPULlama3.java on CratonVM: model load fixed, a Vector API failure still open

## Status
**PARTIALLY FIXED** (2026-08-22). The reported model-load livelock is fixed
and was not a livelock. A second, separate defect on the inference path
remains **OPEN**.

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

## Still open: the first `matmul` throws

With loading fixed, the run reaches inference in ~70 s and dies:

```
gc::guard ERROR: in_published_snapshot=false published_roots=121
  last_publish_at_collection=0  collections_now=1  site="checkcast"
  holder=frame#17 jdk/incubator/vector/AbstractVector.defaultReinterpret
                  pc=35 local[4] kind=0 live=true
  top_frame=jdk/incubator/vector/IntVector.intoMemorySegment0 pc=26

at FloatTensor.matmul(FloatTensor.java:99)
at Parallel.parallelFor(Parallel.java:10)          <-- multi-threaded
at FP16FloatTensor.vectorDot(FP16FloatTensor.java:70)
at jdk/incubator/vector/... (Vector API)
```

**This is NOT yet diagnosed, and specifically it is NOT yet established to
be a GC defect.** The guard's own wording invites that reading, and it has
been wrong before: on 2026-08-17 the identical `in_published_snapshot=false`
signature turned out to be a JIT `multianewarray` bug allocating with
`ClassId(0)`, with no GC involvement, because the guard fires whenever an
address *decodes wrongly*, not only when it was genuinely reclaimed.

Controls run so far:

| control | result |
|---|---|
| `--Xmx 8g` on the quadratic path | no change (that path was never GC-bound) |
| `--Xmx 8g` / `--XX:UseGc G1` on the crash | **not yet run to completion** |
| failure-set diff across runs | **not yet done** |

Before writing this up as a root-collection gap, run the big-heap and
second-collector arms and diff the failure sets. If either arm reproduces,
stop reading the guard and look at what constructed the object — here, the
Vector API's `reinterpret`/`asVectorRaw` type punning is a strong candidate
for producing an object whose header does not decode as its static type.

The one suggestive detail on the GC side: `Parallel.parallelFor` means
worker threads, and `conservative_roots.rs` documents a known
**multi-thread-in-JIT under a peer STW** gap — a worker whose published root
snapshot is stale when a peer collector marks it. `last_publish_at_collection=0`
against `collections_now=1` is consistent with that. Consistent is not
confirmed.

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
