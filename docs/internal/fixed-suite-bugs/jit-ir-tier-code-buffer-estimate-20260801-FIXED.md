# The optimizing tier's code-buffer estimate was never measured

**Status: FIXED — 2026-08-01.** Filed the same day from
`basicerrorcontroller-jit-only-failure-20260731.md` and closed here.

## What was wrong

`jit/src/ir_lower.rs` sized its executable buffer as
`nodes*32 + call_nodes*448 + 1024`, floored at 4096. Booting one Spring Boot
application context exhausted that **56 times in a 4-test class** and 98 times
in a 26-test one, each exhaustion discarding a fully-lowered method and
dropping it to the single-pass backend. `ExecutableBuffer::emit` is
non-panicking, so nothing crashed — this was a silent de-optimization whose
only visible symptom was a flood of thousands of anonymous
`JIT try_patch_i32: offset out of bounds` warnings, one per branch each
abandoned compile still tried to patch.

The original report blamed `x64.rs`. It was the IR tier: 54 optimizing-tier
`code_buffer_exhausted` bailouts against 5 from `x64::compile` in the same run.

## Step 0 — the reported number was lying

`BailoutReason::CodeBufferExhausted` reported `needed: buf.pos()`. `pos()` is
the write cursor, and the cursor **stops at capacity** the moment a buffer
overflows — so it reported `needed == capacity` on every exhausted compile. The
census read `needed 4096 bytes, capacity 4096` seventeen times over and looked
like a tie. It is now `buf.wanted()`, which keeps counting through the dropped
writes; this backend calls neither `rewind_to` nor `emit_checked`, so that is
the exact requirement rather than a bound.

`CRATONVM_DBG_IR_BUFSIZE=1` is the new instrument: one line per IR compile that
reaches emission — `nodes`, `calls`, `wanted`, `capacity`, `overflow` — for
**every** compile, not only the failing ones. An estimate is only as good as its
headroom on the compiles that succeeded, and a census of failures alone cannot
show that.

## Step 1 — the estimate, measured

Census of **1664 IR compiles** across three workloads: Spring Boot's
`BasicErrorControllerDirectMockMvcTests` (595) and
`BasicErrorControllerIntegrationTests` (1067), plus the whole
`bench/CratonBench` CPU suite (2).

| model | overflows | worst under-shoot | reserved |
|---|---|---|---|
| `nodes*32 + calls*448 + 1024`, floor 4K (before) | **155 / 1664** | 2.65x | 6.6 MiB |
| same formula, floor 16K | 5 / 1664 | 1.59x | 26.0 MiB |
| `nodes*64 + calls*1536 + 4096`, floor 8K (**after**) | **0 / 1664** | — | 13.8 MiB |

Two findings the old numbers got wrong, and one they got right:

* **the per-call term was the whole problem.** 448 budgeted; the census puts the
  worst-case marginal cost of a call node at **1141**. The 278 call-free
  compiles never wanted more than 1545 bytes in total, so the per-node term was
  never the issue.
* **the constant was too small to matter.** 1024 does not pay for a prologue,
  an epilogue and frame setup, so every small graph fell through to the floor
  and inherited whatever the floor happened to be — which is why 4096 was where
  the overflows piled up.
* the original comment's premise — "an arithmetic node emits well under 32
  bytes" — was correct. It just was not the term that mattered.

**Raising the floor alone does not work**, and that was the tempting one-line
fix: it still overflows 5 of 1664 while reserving 26 MiB, worse on both axes.
The call-heavy tail wants 25969 bytes; no plausible floor covers that.

**Why not be more generous still.** `ExecutableBuffer::new` adds the full
CAPACITY to `COMMITTED_JIT_CODE_BYTES`, not the bytes used, and that is the
quantity the 256 MiB code-cache cap bounds — over-reserving buys headroom with
code cache. 13.8 MiB for a complete Spring Boot boot is ~5% of the cap.

Five unit tests in `ir_lower::tests` pin the model against the census extremes,
so a future tightening has to argue with the data rather than with a comment.

## Step 2 — the A/B, and what could and could not measure it

`CRATONVM_JIT_IR_LEGACY_BUFFER_ESTIMATE=1` restores the old sizing and the old
floor, so both arms run on **one binary**, alternating run-by-run rather than in
blocks, which is the only way to keep a shared box's load drift from being
confounded with the arm.

### The CPU benchmark suite cannot see this change

The doc asked for the benchmark set to be A/B'd. It was — and the result is
that **it is blind to this work**, which is worth more than the timings:

```
BENCH NEW  ircompiles=2  overflow=0
BENCH OLD  ircompiles=2  overflow=0
```

Two IR compiles in the whole suite, identical in both arms. All seven phases'
checksums match between arms and HotSpot, which is the correctness gate and the
reason to run it. Its **timings differed by 24%** between the arms — entirely
host load (19.6 vs 9.4), since the arms compiled identically. That number is
the useful one to keep: it is a direct measurement of this box's noise floor,
and anything smaller than it on a Spring Boot wall clock means nothing.

### Two synthetic probes failed to reproduce the shape

Recorded so nobody repeats them. `probes/IrBufferTierProbe.java` reproduces the
census's call-heavy graphs, but:

* the first draft used `invokestatic` to monomorphic helpers. Those lower to
  direct calls (~60 bytes), not the MIC + 4-way-PIC inline-cache sites (~360+)
  that make Spring's call nodes expensive; peak `wanted` was 2387 against a
  4096 floor, so **neither arm ever overflowed** and the two arms compiled it
  identically. It measured 1383 ms against 1727 ms and the difference was pure
  noise — a textbook false null, and 25% "slower" in the direction that would
  have looked like a regression.
* switching to megamorphic interface dispatch did not fix it either: the hot
  methods still reached the lowerer with `calls=1`.

The honest conclusion is that the shape is not cheaply synthesisable, and the
real workload is the instrument. **Always `grep -c overflow=true` on both arms
before reading a timing from this probe** — if it is 0 in both, the probe is
measuring nothing.

### The measurement that does engage it

`BasicErrorControllerDirectMockMvcTests` — 590 IR compiles, of which 56 change
tier between the arms (9.4%), the same proportion as the larger class. Quiet
box, load 8.8–14.6, 16 alternating pairs:

| | NEW | OLD |
|---|---|---|
| median wall clock | 30279 ms | 31128 ms |
| IR compiles reaching emission | 590 | 591 |
| discarded on overflow | **0** | **56** |
| tests | 4/4 pass | 4/4 pass |

Median ratio NEW/OLD = **0.973**; paired ratios p25 0.97, median 0.986, p75
1.05, range 0.82–1.13; NEW faster in 9 of 16 pairs.

`BasicErrorControllerIntegrationTests` — 1068 IR compiles, 97–99 changing
tier — points the same way but is **underpowered**: 4 pairs, only one of which
had both members under load 25. All-pairs median ratio 0.807; the clean pair
0.816. Suggestive of an improvement, not evidence of one. One OLD run at load
33.7 failed a test on an `HttpClient request timed out`, which is the same
load artefact this box produces in either arm and not a result.

**Read that as "no regression", not as a speedup.** The paired inter-quartile
range straddles 1.0 and the whole effect is inside the noise floor the bench
arm measured independently. What is not inside the noise is the structural
change: 56 methods per run that used to be thrown away now keep their
optimizing-tier body, and `ircompiles` is unchanged between arms, so the
estimate affects only whether a lowered body survives — not which methods are
admitted.

An earlier round of the same A/B, taken at load 22–35, showed the
`IntegrationTests` arm 1.8x *slower* on 2 of 3 pairs. That was load, and the
quiet-box pairs above reverse it. It is recorded here because it is exactly the
shape of a false positive that would have justified abandoning the change.
