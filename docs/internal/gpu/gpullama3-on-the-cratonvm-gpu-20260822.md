# GPULlama3 on CratonVM's GPU: 1.76x HotSpot, and what it took

**Status: RESOLVED 2026-08-22.** GPULlama3 runs its real inference loop
on CratonVM's GPU offload path and is faster than the same application
on HotSpot, on the same machine, with the same model, producing the
same text.

Opened out of
`gpullama3-model-load-and-ffm-segment-class-identity-RESOLVED-20260822.md`,
whose closing position was that the application reaches inference and
still produces no token, because the Vector API path costs about seven
hours per token. This record is the other route: not making the CPU
path fast, but putting the forward pass on the GPU that was already
there.

## The measurement

`Llama-3.2-1B-Instruct-F16.gguf`, greedy decode (temperature 0), RTX
2060, the application's own `achieved tok/s`. The arms are interleaved
so a busy host cannot favour whichever ran first:

| round | HotSpot CPU | CratonVM GPU |
|---|---|---|
| 1 | 9.89 tok/s | **17.47 tok/s** |
| 2 | 9.74 tok/s | **17.41 tok/s** |
| 3 | 10.25 tok/s | **17.42 tok/s** |

**1.76x.** Both arms generate the same 35 tokens and print the same
text, character for character.

Two things about that comparison are deliberate and both matter.

**The prompt is a single word.** CratonVM's tokenizer truncates a
multi-word prompt — `"Why is the sky blue?"` becomes 11 prompt ids
where HotSpot builds 16, keeping `Why` and dropping ` is`, ` the`,
` sky`, ` blue`, `?`. That is a real defect, it is **pre-existing on
dev** (a binary built before any change in this branch reproduces it
exactly), and it is upstream of everything here: the pretokenizer regex
produces identical pieces on both VMs, so the loss is in BPE
aggregation, and it affects the CPU path identically. Comparing
generated text across VMs on a prompt they tokenize differently would
be comparing answers to two different questions. A one-word prompt
tokenizes identically on both, which makes the comparison sound — and
turns it into a correctness oracle, because greedy decoding from an
identical prompt must produce an identical sequence. It does. See
§Residuals.

**The weight upload is outside the measured window.** It used to run
lazily inside the first forward pass, which put 7 s of one-time setup
inside the interval `RunMetrics` reports as generation. HotSpot's model
load is outside its number, so this has to be outside CratonVM's:
`LlamaApp` now makes the weights resident right after `loadModel`.

## What blocked it

### The lowering could not express a matmul

`out[i] = sum_j w[i*n + j] * x[j]` has a loop-carried accumulator, so
its inner loop must run in order on one thread. The lowering refused
every interior back-edge:

    backward interior branch at pc=51 -> 23 would form a nested loop;
    only the canonical counted-loop back-edge is supported

and its one nested-loop shape — the rectangular 2-D flattening — gives
every `(i, j)` pair its own thread, which computes a different thing.

`loop_recog::classify_outer_parallel_loop` now recognises the outer
loop as the parallel dimension and requires every other back-edge to
lie inside its body; `Emitter::walk_cfg` lowers those as real PTX
loops. It needed less than it looks: the walker already visits blocks
in ascending PC order and already reconciles state at joins, so a
back-edge target is a block whose canonical registers exist and whose
label is already written by the time the edge is emitted — and those
registers ARE the loop's phis. See `../gpu/lowering-nested-loops.md`.

### Half precision, and the one approximation

Weights stay f16 on the device — that is why a 1B model is 2.5 GB and
not 5 — so the kernel needs `Float.float16ToFloat`, lowered to
`cvt.f32.f16`. Exact: widening f16 to f32 has no rounding mode to
choose.

`Math.exp` is the one thing a transformer feed-forward block needs that
the exact intrinsic table cannot build. It lowers to `ex2.approx.f32`
and is admitted only under `CRATONVM_GPU_APPROX_MATH=1`, gated on its
own so a kernel cannot acquire an approximate answer by accident.

## Four defects found by using the feature

Each was found because something downstream was being measured, and
each was silent in the way that matters.

### 1. `MemorySegment.copy` into a heap segment copied nothing

An aliasing heap segment — `MemorySegment.ofArray(byte[]/short[]/
char[])` — has no address. The bulk copy was a raw memmove between two
`segment_address` results guarded by a null check, and when either
address was null it returned normally having done nothing. Fixed by
routing either heap-backed side through the array itself; pinned by
`regression-suite/src/RSegmentBulkCopy.java`, whose fourteen checks
assert CONTENT in all four native/heap directions plus an unaligned
partial-element write.

`ofArray` over `int[]`/`long[]`/`float[]`/`double[]` deliberately does
NOT alias — those allocate an off-heap mirror so the carrier has an
address for downcalls, an asymmetry stated beside the registrations in
`panama.rs` — so the vector asserts the aliasing widths and reaches the
mirror-backed ones through the layout overload instead.

### 2. Reading a segment into an `int[]` ran at 15 MB/s

`MemorySegment.copy(seg, JAVA_INT, off, int[], idx, count)` resolved
the segment — its fields, its scope, its bounds — once **per element**
and then wrote one element. Correct, and 200x slower than HotSpot:
reading a 2.5 GB weight tensor would take three minutes. A contiguous,
native-order, four-byte run now resolves once and goes through the bulk
array write: **15 MB/s -> 5776 MB/s**, against HotSpot's 3047.

### 3. `future.get()` raced the completion reaper

    GpuException: submission handle=35 is Running with no FinalizeState

`finalize_submission` took the `FinalizeState` and released its lock
before doing the work, so the second finalizer — `get()` on the Java
thread against the reaper woken by the device callback — observed the
gap between "state taken" and "status stamped" and declared it a logic
error. It failed about a third of a five-kernel sequence. Fixed by
holding the lock across the finalization.

The cost of this one was not the fix. It was two build cycles spent
looking for an index-arithmetic bug in a kernel whose PTX was correct
all along, because a flaky failure on a brand-new kernel reads as a bad
kernel.

### 4. A failed submission could not say why

`GpuFuture.get()` reads the reason through `Native.futureGetError`,
which was never registered — so every GPU failure surfaced as
`UnsatisfiedLinkError` from inside `get()`. Behind that,
`futureGetErrorMessage` reads the synthetic stub store, which a real
dispatch never writes, and `futureSynchronize` discarded the message
the VM had already computed. All three fixed.

And one documentation claim that was wrong: `docs/gpu/async-api.md`
said the explicit executor path needs no `--gpu` flag because "the
executor probes the driver itself". It does acquire its own
`DeviceContext` — and that is not the context the dispatch uses.
Without `--gpu`, `OffloadCache::new` leaves `ctx = None` and every
submission fails as "not offloadable (Skip)", one call after
`GpuExecutor.open()` has already succeeded.

## Layout and occupancy are worth more than the kernel

RTX 2060, f16 weights, `n = 2048`, effective bandwidth over the weight
bytes read:

| rows | row-major | column-major |
|---|---|---|
| 2048 | 0.9 GB/s | 7.1 GB/s |
| 8192 | 5.1 GB/s | 6.7 GB/s |
| 32768 | 9.4 GB/s | **99.4 GB/s** |

Two effects, and the table separates them. Row-major means consecutive
threads are a whole row apart, so a warp's 32 loads are 32 separate
transactions; transposing fixes that. But the column-major row only
reaches 99 GB/s at 32768 threads — one thread per output row of a
2048-row projection is 64 warps, which on 30 SMs is two warps each and
no way to hide a global-load latency.

So both: every weight is transposed on the device once at load, and the
summed dimension is split so every matmul launches about 32768 threads
(`CratonKernels.matmulSplit` + `reducePartials`). Splitting changes the
order of the summation and is the only kernel in the set whose result
is not bit-identical to the CPU's.

## Correctness

`probes/LlamaKernelChainProbe.java` is the oracle. The kernels are
ordinary static Java methods, so the same source the lowering compiles
to PTX also runs on the host by just calling it: dispatch each kernel
to the device, call it on the CPU with identical inputs, compare lane
by lane. Dimensions are deliberately mismatched (`dim=64, headSize=16,
kvDim=32, hidden=96, ctx=8, vocab=12`) because a kernel that confuses
`headSize` with `kvDim` passes a square fixture.

```
transposeF16   words=2048 diff=0 EXACT
matmulSplit    lanes=256  diff=0 EXACT
reducePartials lanes=64   diff=0 EXACT
embedT[0..3]   lanes=64   diff=0 EXACT
rmsScale       lanes=1    diff=0 EXACT
rmsApply       lanes=64   diff=0 EXACT
rope           lanes=64   diff=0 EXACT
copyTo         lanes=256  diff=0 EXACT
attScores      lanes=32   diff=0 EXACT
addInto        lanes=64   diff=0 EXACT
softmaxRows    lanes=32   worst_rel=1.44e-07 APPROX
attWeighted    lanes=64   worst_rel=5.62e-06 APPROX
siluMul        lanes=96   worst_rel=1.87e-07 APPROX
CHAIN rows=16 exact_failures=0 PASS
```

The three approximate rows are the ones carrying `ex2.approx.f32`;
their disagreement is float rounding. `test_classes/gpu/
EligibleLlamaKernels.java` is a twin of the application's kernel file
(`apps/` is not tracked), so a lowering rejection is caught by
`cargo test` in a second rather than after a model load.

## What is left, and it is most of the time

Per token, measured with `-Dllama.craton.verbose=true`:

| | |
|---|---|
| host time building 453 submissions | **40 ms** |
| device time running them | 5.5 ms |
| logits read-back | 2.5 ms |

The GPU finishes a whole forward pass in 5.5 ms. Nearly everything else
is the per-dispatch floor, measured directly by
`probes/SubmitOverheadProbe.java` at **76 us to submit a kernel that
does nothing**, of which about 47 us is host plumbing and the rest the
driver.

Two rounds of plausible guessing — pooling the one-`u64` failure-flag
buffer, memoising the occupancy query — moved 117 us to 100 us, which
is what guessing usually buys. `CRATONVM_GPU_TIME_DISPATCH=1` now
splits one `submitMethod` and names the cost:

```
gpu dispatch: calls=1521 accounted=63.8 us/call
  read_strings      0.44   read_args       0.15
  lookup_kernel     0.54   marshal_args    3.84
  failure_flag      0.52
  vm_dispatch_all  29.48   future_object  28.78
```

**Half of it is building the `GpuFuture` object.** Every submission
mints a `GpuFutureImpl`, whose constructor allocates two
`AtomicBoolean`s, a `ReentrantReadWriteLock` and a `Cleaner`
registration — five objects and a phantom reference, interpreted, for a
future that 452 of the 453 callers immediately discard. The remaining
~25 us is the launch itself (`Event::new`, `launch_on_stream`, the host
callback, submission registration).

So the named next lever is a fire-and-forget submission that returns a
handle instead of a future object, plus an event pool. If the floor
went to 10 us the forward pass would be about 13 ms per token rather
than 48 — roughly 75 tok/s, against the 5.5 ms the device actually
spends. That is a change to the `craton-gpu-java` API surface rather
than to CratonVM, which is why it is written down here rather than
done.

## Residuals

* **The tokenizer truncates a multi-word prompt.** 11 prompt ids where
  HotSpot builds 16; the pretokenizer regex agrees on both VMs, so the
  loss is in BPE aggregation. Pre-existing on dev, upstream of the GPU
  path, and it affects the CPU path identically.
  `probes/Llama3PretokenizeProbe.java` and
  `probes/SpecialTokenSplitProbe.java` rule out the two obvious causes
  (the regex itself, and `Pattern.quote` + `String.split`).
* **`MemorySegment.ofArray(int[])` does not alias its array.** By
  design today — see §1 — but it means `MemorySegment.copy` INTO such a
  segment writes the mirror and leaves the array untouched, silently.
  `probes/HeapSegmentShapeProbe.java` shows it in four lines against a
  HotSpot control.

## Repro

```bash
cd apps/GPULlama3.java
bash compare-hotspot-vs-craton.sh <cratonvm.exe> "Why" 96
```

```bash
CRATONVM_GPU_APPROX_MATH=1        # admits Math.exp, and only Math.exp
CRATONVM_GPU_TIME_DISPATCH=1      # the per-dispatch census above
-Dllama.craton.gpu=true           # selects the GPU forward pass
-Dllama.craton.verbose=true       # per-token submit/drain/readback split
-Dllama.craton.targetThreads=N    # split-matmul thread target (default 32768)
-Dllama.craton.syncEach=true      # one host wait per kernel, for bisecting
```

The GPU build is `CARGO_TARGET_DIR=target-gpu cargo build --release -p
cratonvm-cli --bin cratonvm --features gpu-driver`, and `--gpu` is
required on the command line even though the path is the explicit
executor API.
