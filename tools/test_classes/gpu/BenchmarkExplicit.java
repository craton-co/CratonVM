// Benchmark for the Phase 5–7 explicit-submit GPU dispatch path.
//
// Unlike `Benchmark.java`, which leans on the interpreter's
// transparent `--gpu` interception of `EligibleVectorAdd.vectorAdd`,
// this driver uses `craton.gpu.GpuExecutor` directly:
//
//   try (GpuExecutor exec = GpuExecutor.open()) {
//       GpuFuture<Void> f = exec.submit(
//           "EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out);
//       f.get();
//   }
//
// This exercises Phase 5's `Native.submitMethod` → `dispatch_method_
// from_native` → real cudarc launch (Phase 3.5) → Phase 7 #1's
// deferred finalization on `future.get()`. The transparent path
// stays available for `EligibleVectorAdd.vectorAdd`-style calls but
// is independent of this fixture's submission pattern.
//
// Usage (CUDA-equipped machine):
//   javac -cp target/<build>/.../craton-gpu-annotations.jar \
//         -d test_classes/gpu \
//         test_classes/gpu/BenchmarkExplicit.java \
//         test_classes/gpu/EligibleVectorAdd.java
//   cratonvm --gpu \
//           --classpath test_classes/gpu:<craton-gpu-jar> \
//           BenchmarkExplicit 16777216 5
//
// The craton-gpu jar lives in the cargo build dir; the run script
// in docs/gpu/first-results.md spells out the exact path. If the
// `craton.gpu.GpuExecutor` class isn't on the classpath, this
// driver exits with a clear diagnostic instead of crashing.

import craton.gpu.GpuExecutor;
import craton.gpu.GpuException;
import craton.gpu.GpuFuture;

public class BenchmarkExplicit {
    public static void main(String[] args) {
        int n          = (args.length > 0) ? Integer.parseInt(args[0]) : (1 << 24);
        int iterations = (args.length > 1) ? Integer.parseInt(args[1]) : 5;
        int warmup     = (args.length > 2) ? Integer.parseInt(args[2]) : 2;

        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];

        long seed = 0xC0FFEEL;
        for (int i = 0; i < n; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            a[i] = (int) (seed >>> 32);
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            b[i] = (int) (seed >>> 32);
        }

        try (GpuExecutor exec = GpuExecutor.open()) {

            // Warmup: drives OffloadCache compile + first device
            // allocation. Phase 7 #2 keeps the result cached for
            // subsequent calls (but only for the resident-GpuArray
            // path — plain int[] args still upload each call).
            for (int w = 0; w < warmup; w++) {
                GpuFuture<Void> f = exec.submit(
                    "EligibleVectorAdd",
                    "vectorAdd",
                    "([I[I[I)V",
                    a, b, out
                );
                f.get();
            }

            long[] timings = new long[iterations];
            int reference0 = out[0];
            int referenceN = out[n - 1];

            for (int i = 0; i < iterations; i++) {
                long t0 = System.nanoTime();
                GpuFuture<Void> f = exec.submit(
                    "EligibleVectorAdd",
                    "vectorAdd",
                    "([I[I[I)V",
                    a, b, out
                );
                f.get();
                long t1 = System.nanoTime();
                timings[i] = t1 - t0;

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
        } catch (GpuException e) {
            System.out.println("GpuException: " + e.getMessage());
            System.exit(2);
        } catch (NoClassDefFoundError e) {
            System.out.println(
                "NoClassDefFoundError: " + e.getMessage()
                + " (the craton-gpu jar is probably missing from --classpath; "
                + "see docs/gpu/first-results.md for the exact path)"
            );
            System.exit(3);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            System.out.println("interrupted");
            System.exit(4);
        }
    }
}
