import craton.gpu.AdmissionHint;
import craton.gpu.GpuArray;
import craton.gpu.GpuExecutor;
import craton.gpu.GpuKernel;

/**
 * What does one `exec.submit` cost when the kernel itself costs
 * nothing and every argument is already device-resident?
 *
 * An inference step is hundreds of tiny kernels, so this per-call floor
 * is multiplied by ~450 before anything else is measured. The kernel
 * below writes one element per thread over a 64-element array: the
 * device work is a rounding error, so the number IS the dispatch cost.
 *
 * Two arms, because they answer different questions: submit-and-wait
 * gives the round-trip a `syncEach` caller pays, and submit-many-then-
 * wait-once gives the marginal cost inside a batch, which is what an
 * inference step actually pays.
 */
public class SubmitOverheadProbe {

    @GpuKernel(admit = AdmissionHint.ALLOW_INTRINSIC_CALLS)
    public static void touch(float[] a, float[] out) {
        int n = out.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + 1.0f;
        }
    }

    public static void main(String[] args) throws Exception {
        int iters = Integer.parseInt(System.getProperty("iters", "500"));
        try (GpuExecutor exec = GpuExecutor.open()) {
            GpuArray<float[]> a = GpuArray.wrap(new float[64]);
            GpuArray<float[]> b = GpuArray.wrap(new float[64]);
            for (int i = 0; i < 20; i++) {
                exec.submit("SubmitOverheadProbe", "touch", "([F[F)V", a, b).get();
            }

            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                exec.submit("SubmitOverheadProbe", "touch", "([F[F)V", a, b).get();
            }
            long each = (System.nanoTime() - t0) / iters;
            System.out.printf("SUBMIT_AND_WAIT ns=%d%n", each);

            var last = exec.submit("SubmitOverheadProbe", "touch", "([F[F)V", a, b);
            long t1 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                last = exec.submit("SubmitOverheadProbe", "touch", "([F[F)V", a, b);
            }
            long queued = (System.nanoTime() - t1) / iters;
            last.get();
            System.out.printf("SUBMIT_ONLY     ns=%d%n", queued);

            // Same plumbing, no driver: a class that does not exist
            // fails inside the native before any CUDA call, so this
            // isolates string reads, argument conversion, the future
            // object, and the Java wrapper from the GPU itself.
            long t2 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                exec.submit("NoSuchKernelClass", "touch", "([F[F)V", a, b);
            }
            System.out.printf("SUBMIT_NO_GPU   ns=%d%n", (System.nanoTime() - t2) / iters);

            // The fire-and-forget path: no GpuFuture object per call.
            Object[] fafArgs = new Object[] {a, b};
            long h = 0;
            for (int i = 0; i < 20; i++) {
                h = exec.dispatchNamedHandle("SubmitOverheadProbe", "touch", "([F[F)V", fafArgs);
            }
            exec.awaitSubmission(h);
            long t4 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                h = exec.dispatchNamedHandle("SubmitOverheadProbe", "touch", "([F[F)V", fafArgs);
            }
            long queuedFaf = (System.nanoTime() - t4) / iters;
            exec.awaitSubmission(h);
            System.out.printf("HANDLE_ONLY     ns=%d%n", queuedFaf);

            long t5 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                exec.awaitSubmission(exec.dispatchNamedHandle(
                        "SubmitOverheadProbe", "touch", "([F[F)V", fafArgs));
            }
            System.out.printf("HANDLE_AND_WAIT ns=%d%n", (System.nanoTime() - t5) / iters);

            // And a pure-Java control on the same objects: the cost of
            // CratonVM running the wrapper at all.
            long sink = 0;
            long t3 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                sink += exec.handleForDispatch() + a.length() + b.length();
            }
            System.out.printf("JAVA_CONTROL    ns=%d sink=%d%n",
                    (System.nanoTime() - t3) / iters, sink);
        }
    }
}
