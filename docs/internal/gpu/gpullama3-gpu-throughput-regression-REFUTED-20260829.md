# The "GPULlama3 regressed 42% after the bulk-marshal / GEMM-tile changes" was the host CPU

**Status: REFUTED 2026-08-29.** There is no regression. The two binaries
were re-measured interleaved on the same host, and the one accused of being
42% slower is 5% FASTER. What the original record compared was two windows
an hour apart on a host whose throughput on this workload moves by 2x
between windows -- a larger difference than any change anyone has landed on
this path. The mechanism underneath that is CratonVM's GPU path being
host-dispatch-bound, which this tree wrote down on 2026-08-23 and which
section 3 reproduces on demand.

Supersedes the open record
`gpullama3-gpu-throughput-regression-after-bulk-marshal-20260829.md`, written
the same morning under `docs/known-issues/gpu/`. That page was never
committed; this one replaces it, and the commit that adds this one removes it
from the working tree.

---

## 1. What the record claimed

Same host, same RTX 2060, same command, only the binary changed:

| build | dev tip | achieved tok/s |
|---|---|---|
| built 08:22 | before `8cf39edee` | 20.24, 21.35, 20.26 (avg **20.6**) |
| rebuilt 09:43 | after the bulk-marshal + GEMM merges | 11.32, 12.33, 11.37, 12.23, 12.16 (avg **11.9**) |

"~42% slower", "reproduced 5x in isolation, no host contention". Output was
byte-identical on both, so it was recorded as a pure throughput regression.

Both blocks are real measurements. What makes them not an A/B is that all
of arm A ran in one block and all of arm B in another, an hour apart, on a
host that does not hold still for an hour — the same design flaw
`bench-gpu/run-raytracer-interleaved.sh` exists to avoid, and whose header
comment says so in as many words. "Reproduced 5x in isolation" is not a
defence: five consecutive runs inside one window all see that window.

## 2. The interleaved re-measurement

Arm A is a fresh `--features gpu-driver` build of `c86bc71c5`, the dev tip
immediately before both GPU merges. Arm B is the **exact 09:43 binary the
record measured** — it was still on disk, so this is not a rebuild that
might differ. Six rounds, arm order alternated per round, nothing else
running:

| round | A (pre-merge) | B (the accused 09:43 build) | order |
|---|---:|---:|---|
| 1 | 21.38 | 22.22 | A-then-B |
| 2 | 20.83 | 21.78 | B-then-A |
| 3 | 20.25 | 21.68 | A-then-B |
| 4 | 21.78 | 22.87 | B-then-A |
| 5 | 20.41 | 23.49 | A-then-B |
| 6 | 22.28 | 21.42 | B-then-A |
| **mean** | **21.16** | **22.24** | |

**B is 1.05x FASTER**, and wins 5 of 6 rounds. The binary that measured
11.9 tok/s an hour earlier measures 22.2 tok/s here, unchanged, on the same
machine with the same command.

That direction is also the one the code predicts. GPULlama3's CratonVM path
does not use the built-in GEMM kernels at all — `CratonGpu` dispatches
`CratonKernels` methods lowered from bytecode, and `OffloadCache::
builtin_module` loads the GEMM PTX lazily on the first `gemm` submit, which
never happens — so `d51ae1eab` and `c9a7a2b13` cannot reach this workload.
`8cf39edee` can: `logits.toHost` hands back a 128,256-element float row per
token, which that commit turned from 128,256 `set_array_element` calls into
one memcpy. The measured `readback_ms` is 0.3 ms.

## 3. The mechanism, reproduced on demand

The record's numbers are not noise — they are a real regime, and it can be
entered deliberately. Same binary (B), same prompt, per-token breakdown from
the application's own `-Dllama.craton.verbose=true`:

| host state | submit_ms | drain_ms | readback_ms | achieved tok/s |
|---|---:|---:|---:|---:|
| idle | 13.0 – 14.6 | 23.5 – 24.9 | 0.3 | **23.3** |
| 24 CPU spinners on 32 cores | 54.7 – 62.1 | 4.8 – 5.4 | 0.5 – 0.8 | **13.2** |

13.2 tok/s is the record's regime, produced from a binary that is not
slower, by taking CPU away from it.

**That is a sufficient cause, not a proof of what happened at 09:43.**
Later the same day, on a box with nothing else running, six interleaved
rounds of the same pair produced arm means spanning 11.6 to 22.6 tok/s
while a fixed single-threaded HotSpot CPU render measured in the same
rounds held within 5% (23.0 to 24.2 ms). So the host's CPU clock is not the
only thing that moves this workload. An idle RTX 2060 on this box sits at
`P8` and 300 MHz against a 2100 MHz maximum, and a 32-token run is 1.4 s of
device work; whether that ramp accounts for the rest was not separated
here. What is established is the pair of facts that matter: the accused
binary is not slower, and the regime the record recorded is reachable from
host state alone.

Read the two middle columns together, because they are the whole finding:

* `submit_ms` is HOST time spent building the token's 453 kernel
  submissions. It went 14 → 60 ms, a 4.3x, and that alone is larger than
  the entire idle token.
* `drain_ms` is what is left of device time after the host stops
  submitting. It went **down**, 24 → 5 ms. The device did not get slower.
  It got starved: by the time the host had finished issuing the chain,
  the GPU had already run it.

So the arm that looks 42% slower is not doing less work per second on the
GPU — it is failing to feed it. This is the same shape as the ray tracer
record's 2026-08-23 observation ("TornadoVM moved 15.0 -> 18.4 while
CratonVM went 19.2 -> 10.5; `drain_ms` stayed at 5-6 ms throughout —
TornadoVM is GPU-bound, CratonVM is host-dispatch-bound"), now measured
per-token with the two halves separated.

## 4. What is actually worth fixing here

Not a regression: the dispatch cost itself. 453 submissions per token at
~31 µs of host time each is 14 ms, 35% of an idle token and more than 100%
of a contended one. Two driver round trips per submission were pure
bookkeeping and are gone as of this record's own commit — see
`gpu/raytracer-vs-tornadovm-RESOLVED-20260821.md` §13.5 for the event-pool
and same-stream-wait-elision work, its engagement census, and why the ray
tracer -- 8 launches a frame against this workload's 453 a token -- was
never going to show it.

## 5. Repro

```bash
# The interleaved A/B. Never compare two GPU binaries in separate blocks
# on this host.
A=<pre-merge cratonvm.exe> B=<post-merge cratonvm.exe> ROUNDS=6 \
  bash bench-gpu/run-gpullama3-ab.sh

# The same pair on host dispatch cost alone, with a per-round CPU control,
# a GPU clock reading, and a count of the other builds on the box.
A=... B=... ROUNDS=6 bash bench-gpu/run-gpullama3-submit-ab.sh

# The mechanism, one binary, two host states.
cd apps/GPULlama3.java
CRATON_EXTRA="-Dllama.craton.verbose=true" \
  bash run-craton-gpu.sh <cratonvm.exe> -p "Why is the sky blue?" -n 32
# then again with `for i in $(seq 1 24); do (while :; do :; done) & done`
```

## 6. The rule this cost an afternoon to re-learn

A CratonVM GPU number from this box is meaningful only beside the arm it is
being compared against, measured in the same minutes. Interleave, or do not
compare. `bench-gpu/run-gpullama3-ab.sh` and
`bench-gpu/run-gpullama3-submit-ab.sh` exist so that is the easy thing to
do; the second also prints the GPU's clock and how many other compiler
processes are running, because this machine is shared between concurrent
sessions and four other `rustc` processes have been observed
mid-measurement.
