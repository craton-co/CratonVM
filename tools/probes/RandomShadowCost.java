import java.util.Random;

/**
 * Prices what retiring the `java.util.Random` NATIVE SHADOW could buy, without
 * changing the VM.
 *
 * <p>`jpalargeblob-random-state-side-table-20260829.md`'s mechanism 2 is that
 * the fixture's `read()` costs five native calls per byte:
 * `Random.<init>`, `Random.nextInt`, two `Long.longValue` and one
 * `Long.valueOf`. Its "not yet done" list proposes dropping the `Random`
 * natives in real-JDK mode, where the JDK's own pure-Java implementation is
 * correct and JIT-compilable — but notes the A/B is blocked because the dial
 * that would disable the shadow is scoped to `--jdk-only`.
 *
 * <p>It is not blocked. `MyRandom` below is the same LCG the native implements
 * (`seed = (seed * 0x5DEECE66D + 0xB) & ((1 << 48) - 1)`), written in Java, so
 * it goes through the JIT exactly as the real JDK's would. The gap between the
 * two arms is an UPPER BOUND on what retiring the shadow can win, measured on
 * the binary that exists today.
 *
 * <p>The boxing arms are here for proportion: they are the other three of the
 * five calls, and no change to `Random` touches them.
 */
public class RandomShadowCost {

    static final int ITERS = Integer.getInteger("iters", 300_000);

    /** The JDK's documented LCG, in Java. Same sequence as java.util.Random. */
    static final class MyRandom {
        private long seed;
        MyRandom(long s) { this.seed = (s ^ 0x5DEECE66DL) & ((1L << 48) - 1); }
        int next(int bits) {
            seed = (seed * 0x5DEECE66DL + 0xBL) & ((1L << 48) - 1);
            return (int) (seed >>> (48 - bits));
        }
        int nextInt() { return next(32); }
    }

    static long sink;

    static void arm(String name, Runnable body) {
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < 3; rep++) {
            long t0 = System.nanoTime();
            body.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) { best = dt; }
        }
        System.out.printf("CK %-34s %9.1f ns/op%n", name, (double) best / ITERS);
    }

    public static void main(String[] args) {
        // Warm up so every arm is measured compiled.
        for (int i = 0; i < ITERS; i++) { sink += new Random(i).nextInt(); }
        for (int i = 0; i < ITERS; i++) { sink += new MyRandom(i).nextInt(); }

        System.out.println("CK RandomShadowCost iters=" + ITERS);

        arm("new Random(i).nextInt()   NATIVE", () -> {
            for (int i = 0; i < ITERS; i++) { sink += new Random(i).nextInt(); }
        });
        arm("new MyRandom(i).nextInt() JAVA", () -> {
            for (int i = 0; i < ITERS; i++) { sink += new MyRandom(i).nextInt(); }
        });

        Random shared = new Random(1);
        MyRandom mine = new MyRandom(1);
        arm("shared Random.nextInt()   NATIVE", () -> {
            for (int i = 0; i < ITERS; i++) { sink += shared.nextInt(); }
        });
        arm("shared MyRandom.nextInt() JAVA", () -> {
            for (int i = 0; i < ITERS; i++) { sink += mine.nextInt(); }
        });

        // The other three of the five calls per byte, for proportion.
        arm("boxed Long counter", () -> {
            Long count = (long) ITERS;
            long acc = 0;
            for (int i = 0; i < ITERS; i++) {
                if (count > 0) { count--; acc += 1; }
            }
            sink += acc;
        });
        arm("primitive long counter", () -> {
            long count = ITERS;
            long acc = 0;
            for (int i = 0; i < ITERS; i++) {
                if (count > 0) { count--; acc += 1; }
            }
            sink += acc;
        });

        System.out.println("CK RandomShadowCost sink=" + (sink == 0 ? 0 : 1));
    }
}
