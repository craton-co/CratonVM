# CratonBench arithmetic performance regression

**Status: OPEN — observed regression; controlled reproduction is still needed.**

The published arithmetic rows in [`BENCHMARK.md`](../../../BENCHMARK.md#1-arithmetic)
show CratonVM at 1.18–1.19× HotSpot. A three-round rerun on 2026-11-12 measured
the same benchmark at 6.67–8.09× HotSpot in both CratonVM modes. Checksums
matched on every sample. This is a performance discrepancy, not a correctness
failure.

## Results

Times are medians in milliseconds. “Foundational” is the launcher's default
mode; “compatible” uses `--compatible`.

| Iterations | Published ratio | G1: HotSpot / foundational / compatible | ZGC: HotSpot / foundational / compatible | Compatible ratio, G1 / ZGC |
|---:|---:|---:|---:|---:|
| 100M | 1.18× | 127 / 926 / 927 | 127 / 927 / 922 | 7.30× / 7.26× |
| 1B | 1.19× | 1,145 / 9,277 / 9,264 | 1,142 / 9,254 / 9,238 | 8.09× / 8.09× |
| 10B | 1.19× | 11,250 / 75,123 / 75,080 | 11,205 / 74,791 / 75,121 | 6.67× / 6.70× |

The result is consistent across collectors and modes, so the mode toggle does
not explain the gap. The 3.5–3.6 median one-minute load average during the
arithmetic samples makes the timings noisy and may have amplified the result.

## Measurement details

- Three fresh-process repetitions per configuration, interleaved across
  HotSpot, foundational CratonVM and compatible CratonVM; CPU affinity was
  pinned to CPU 2.
- OpenJDK 25.0.5.0.3 on the shared `64.177.43.150` Linux host.
- Results metadata records source revision `fe4af63a8c` and CratonVM binary
  SHA-256 `e2207dbaf50c0046527bedc601c05bfbebd19162044628aa7948c7a0efd8dd25`.
- Raw samples and checksums: `/root/published-ci-full-20261112/results.json`.
- Reproduction harness: [`run-published-ci.py`](../../../tools/bench/run-published-ci.py).

The published table used OpenJDK 25.0.4 on an 8-vCPU host. This rerun used a
newer JDK patch and a different shared-host load profile. Repeat the arithmetic
rows with the published JDK and a quiet, pinned host before replacing the
published medians or attributing the slowdown to a code change.
