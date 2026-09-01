import java.util.*;

/** L3 — what retiring `java.util.Random`'s 32 shadows COSTS.
 *
 *  `apps/probes/RandomShadowSweep` says the retirement is free in correctness:
 *  51 rows, 0-diff in both modes, with `CRATONVM_JDK_RANDOM=1` removing all 32
 *  registrations. A correctness probe cannot see a throughput regression, and
 *  `java.util.Random`'s real body is a CAS loop on an `AtomicLong` where the
 *  native is a plain field, so this is the axis where the shadow could be
 *  earning its keep.
 *
 *  It prints ns/op, not a verdict. The caller runs it with the flag off and on
 *  and compares; the numbers are only meaningful interleaved on an idle host,
 *  which is why the runner does A/B/B/A rather than A then B.
 *
 *  The accumulator is printed so the loop cannot be optimised away, and each
 *  measured section is preceded by a warmup of the same shape so the JIT has
 *  compiled it before the clock starts.
 */
public class RandomBench {
    static final int WARMUP = 2_000_000;
    static final int ITERS = 8_000_000;

    static long benchNextInt() {
        Random r = new Random(12345);
        int acc = 0;
        for (int i = 0; i < WARMUP; i++) acc += r.nextInt();
        long t0 = System.nanoTime();
        for (int i = 0; i < ITERS; i++) acc += r.nextInt();
        long dt = System.nanoTime() - t0;
        if (acc == 0x7fffffff) System.out.print("");
        return dt;
    }

    static long benchNextIntBound() {
        Random r = new Random(12345);
        int acc = 0;
        for (int i = 0; i < WARMUP; i++) acc += r.nextInt(1000);
        long t0 = System.nanoTime();
        for (int i = 0; i < ITERS; i++) acc += r.nextInt(1000);
        long dt = System.nanoTime() - t0;
        if (acc == 0x7fffffff) System.out.print("");
        return dt;
    }

    static long benchNextDouble() {
        Random r = new Random(12345);
        double acc = 0;
        for (int i = 0; i < WARMUP; i++) acc += r.nextDouble();
        long t0 = System.nanoTime();
        for (int i = 0; i < ITERS; i++) acc += r.nextDouble();
        long dt = System.nanoTime() - t0;
        if (acc == 12345.6789) System.out.print("");
        return dt;
    }

    public static void main(String[] args) {
        long a = benchNextInt();
        long b = benchNextIntBound();
        long c = benchNextDouble();
        System.out.println("nextInt      ns/op " + (a / ITERS));
        System.out.println("nextInt(1000) ns/op " + (b / ITERS));
        System.out.println("nextDouble   ns/op " + (c / ITERS));
        System.out.println("DONE RandomBench");
    }
}
