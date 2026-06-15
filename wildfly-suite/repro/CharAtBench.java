// Bug-03 microbench: isolates the two JIT tier-up paths for String.charAt/length.
//   * "method"  line: charAt loop inside a helper CALLED many times  -> invocation
//                      threshold -> try_jit_upgrade_with_gate (now intrinsified).
//   * "OSR"     line: charAt loop nested in main(), single invocation -> backedge
//                      threshold -> try_osr (not wired to String intrinsics yet).
// Compare CratonVM (before/after the wiring) against HotSpot.
public class CharAtBench {
    // Called many times -> invocation-compiled via the main gate.
    static long sum(String s) {
        long h = 0;
        int n = s.length();
        for (int i = 0; i < n; i++) h = h * 31 + s.charAt(i);
        return h;
    }

    public static void main(String[] a) {
        String s = "org.junit.jupiter.engine.descriptor.ClassBasedTestDescriptor"; // 58 chars
        final int CALLS = 200_000;
        long acc = 0;

        // Warm up the method-call path (let the gate compile sum()).
        for (int w = 0; w < CALLS; w++) acc += sum(s);
        long t0 = System.nanoTime();
        for (int i = 0; i < CALLS; i++) acc += sum(s);
        long t1 = System.nanoTime();
        System.out.println("charAt via method x" + CALLS + " (~" + ((long) CALLS * s.length())
                + " charAt) = " + (t1 - t0) / 1_000_000 + "ms acc=" + acc);

        // OSR-shaped: one invocation, one hot nested loop.
        int n = s.length();
        long h2 = 0;
        long t2 = System.nanoTime();
        for (int r = 0; r < CALLS; r++) {
            for (int i = 0; i < n; i++) h2 += s.charAt(i);
        }
        long t3 = System.nanoTime();
        System.out.println("charAt single-loop (OSR) (~" + ((long) CALLS * n)
                + " charAt) = " + (t3 - t2) / 1_000_000 + "ms h=" + h2);
    }
}
