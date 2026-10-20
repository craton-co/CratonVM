// Interpreter round i1, wave 4, lane L2 — `ldc2_w` hot-path measurement.
//
// `ldc2_w` of a CONSTANT_Long / CONSTANT_Double used to take the class_manager
// read lock and look the class up on every execution; it now answers a warm
// site from a per-thread table (`site_cache::Ldc2wSiteCache`).
//
// What to measure: the per-iteration time printed on STDERR, with the JIT off
// (CRATONVM_DISABLE_JIT=1), before/after, and with
// CRATONVM_JIT_NO_LDC_CONST_CACHE=1 as the in-binary A/B arm. Interleave the
// arms and take medians (in-JVM timings on this host swing ~3x between reps).
// The `mt` row runs the same loop on 4 threads, where the shared read lock was
// the contended part.
//
// Expected stdout (HotSpot 25), identical with and without the cache:
//   st checksum=-1612708855070181376 4212a05f200275ea
//   mt checksum=-6450835420280725504 4232a05f200275ea
public class L2Ldc2wBench {
    static final int N = 2_000_000;

    // javac emits ldc2_w for every long/double literal other than 0/1 (0.0/1.0).
    static long lsum;
    static double dsum;

    static void loop() {
        long l = 0;
        double d = 0;
        for (int i = 0; i < N; i++) {
            l += 0x1234_5678_9ABCL;
            l ^= 0x0F0F_0F0F_0F0F_0F0FL;
            d += 10_000.0000005;
        }
        synchronized (L2Ldc2wBench.class) {
            lsum += l;
            dsum += d;
        }
    }

    public static void main(String[] a) throws Exception {
        for (int rep = 0; rep < 5; rep++) {
            lsum = 0;
            dsum = 0;
            long t0 = System.nanoTime();
            loop();
            long t1 = System.nanoTime();
            System.err.printf("st rep %d: %.2f ns/iter%n", rep, (t1 - t0) / (double) N);
        }
        System.out.println("st checksum=" + lsum + " " + Long.toHexString(Double.doubleToLongBits(dsum)));

        for (int rep = 0; rep < 3; rep++) {
            lsum = 0;
            dsum = 0;
            Thread[] ts = new Thread[4];
            long t0 = System.nanoTime();
            for (int k = 0; k < ts.length; k++) {
                ts[k] = new Thread(L2Ldc2wBench::loop);
                ts[k].start();
            }
            for (Thread t : ts) {
                t.join();
            }
            long t1 = System.nanoTime();
            System.err.printf("mt rep %d: %.2f ns/iter (4 threads)%n", rep, (t1 - t0) / (double) N);
        }
        System.out.println("mt checksum=" + lsum + " " + Long.toHexString(Double.doubleToLongBits(dsum)));
    }
}
