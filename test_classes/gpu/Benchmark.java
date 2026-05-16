// Benchmark driver for the GPU-offload end-to-end acceptance gate
// (see docs/gpu/cuda-oxide-evaluation.md, Part J of the plan).
//
// Usage on a CUDA-equipped machine:
//   javac -d test_classes/gpu test_classes/gpu/Benchmark.java
//                                      test_classes/gpu/EligibleVectorAdd.java
//   rustjvm --classpath test_classes/gpu Benchmark 16777216           # CPU baseline
//   rustjvm --gpu --classpath test_classes/gpu Benchmark 16777216     # GPU
//
// Output format (machine-parseable):
//   mode=cpu|gpu n=<N> elapsed_ns=<ELAPSED> out[0]=<V0> out[n-1]=<VN1>
//
// Acceptance criteria from the plan:
//   - Both runs produce identical out[0] and out[n-1] (correctness).
//   - GPU elapsed_ns strictly less than CPU elapsed_ns at n >= 1<<24.
public class Benchmark {
    public static void main(String[] args) {
        int n = (args.length > 0) ? Integer.parseInt(args[0]) : (1 << 24);
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];

        // Deterministic seed so CPU and GPU runs operate on identical
        // input data without relying on java.util.Random (which the
        // GPU-eligibility analyzer would reject if it appeared mid-method).
        long seed = 0xC0FFEEL;
        for (int i = 0; i < n; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            a[i] = (int) (seed >>> 32);
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            b[i] = (int) (seed >>> 32);
        }

        long t0 = System.nanoTime();
        EligibleVectorAdd.vectorAdd(a, b, out);
        long t1 = System.nanoTime();
        long elapsed = t1 - t0;

        System.out.println(
            "mode=auto n=" + n
            + " elapsed_ns=" + elapsed
            + " out[0]=" + out[0]
            + " out[n-1]=" + out[n - 1]
        );
    }
}
