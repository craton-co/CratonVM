// Benchmark driver for the GPU-offload end-to-end acceptance gate
// (Part J of the original plan; updated for Phase 5–8 paths).
//
// Modes:
//   transparent  — `--gpu` is set on the CLI and the JVM's
//                  `execute_invokestatic` hook intercepts the call to
//                  `EligibleVectorAdd.vectorAdd` and dispatches it via
//                  `OffloadCache::try_dispatch` (Phase 1 Part E path).
//   cpu          — no `--gpu` flag; the call runs on the interpreter.
//
// Usage on a CUDA-equipped machine:
//   javac -d test_classes/gpu test_classes/gpu/Benchmark.java
//                                      test_classes/gpu/EligibleVectorAdd.java
//   cratonvm        --classpath test_classes/gpu Benchmark 16777216 5   # CPU baseline
//   cratonvm --gpu  --classpath test_classes/gpu Benchmark 16777216 5   # GPU
//
// Arguments:
//   args[0]  n            — element count (default 1<<24 = 16,777,216)
//   args[1]  iterations   — measurement iterations after warmup (default 5)
//   args[2]  warmup       — warmup iterations before measurement (default 2)
//
// Output format (machine-parseable; one line per iteration plus a
// summary line):
//
//   iter=<I> elapsed_ns=<NS> out[0]=<V0> out[n-1]=<VN1>
//   ...
//   summary n=<N> iterations=<I> min_ns=<MIN> mean_ns=<MEAN> max_ns=<MAX>
//
// Acceptance criteria from the plan:
//   - Correctness: `out[0]` and `out[n-1]` are identical across all
//     iterations AND between CPU and GPU runs.
//   - Speedup: GPU `mean_ns` at n >= 1<<24 should be strictly less
//     than the CPU mean. Target ≥ 2×; smaller is a follow-up topic,
//     not a fail.
//   - No regression: `cargo check --workspace` clean both with and
//     without `--features cratonvm-vm/gpu-offload`.
public class Benchmark {
    public static void main(String[] args) {
        int n          = (args.length > 0) ? Integer.parseInt(args[0]) : (1 << 24);
        int iterations = (args.length > 1) ? Integer.parseInt(args[1]) : 5;
        int warmup     = (args.length > 2) ? Integer.parseInt(args[2]) : 2;

        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];

        // Deterministic input so CPU and GPU runs operate on identical
        // data without relying on java.util.Random (which the
        // GPU-eligibility analyzer would reject if it appeared mid-
        // method, and which would also drift between JVMs).
        long seed = 0xC0FFEEL;
        for (int i = 0; i < n; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            a[i] = (int) (seed >>> 32);
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            b[i] = (int) (seed >>> 32);
        }

        // Warmup: discard timings. On the GPU path this populates the
        // OffloadCache for `EligibleVectorAdd.vectorAdd`; subsequent
        // iterations skip the analyze + lower + load steps.
        for (int w = 0; w < warmup; w++) {
            EligibleVectorAdd.vectorAdd(a, b, out);
        }

        long[] timings = new long[iterations];
        int reference0 = out[0];        // post-warmup reference value
        int referenceN = out[n - 1];

        for (int i = 0; i < iterations; i++) {
            long t0 = System.nanoTime();
            EligibleVectorAdd.vectorAdd(a, b, out);
            long t1 = System.nanoTime();
            timings[i] = t1 - t0;

            // Correctness check per iteration: kernel output must be
            // stable across runs given identical input.
            if (out[0] != reference0 || out[n - 1] != referenceN) {
                System.out.println(
                    "FAIL iter=" + i + " out[0]=" + out[0]
                    + " reference0=" + reference0
                    + " out[n-1]=" + out[n - 1]
                    + " referenceN=" + referenceN
                );
                System.exit(1);
            }

            System.out.println(
                "iter=" + i
                + " elapsed_ns=" + timings[i]
                + " out[0]=" + out[0]
                + " out[n-1]=" + out[n - 1]
            );
        }

        long min  = Long.MAX_VALUE;
        long max  = 0L;
        long total = 0L;
        for (long t : timings) {
            if (t < min) min = t;
            if (t > max) max = t;
            total += t;
        }
        long mean = total / iterations;

        System.out.println(
            "summary"
            + " n=" + n
            + " iterations=" + iterations
            + " warmup=" + warmup
            + " min_ns=" + min
            + " mean_ns=" + mean
            + " max_ns=" + max
        );
    }
}
