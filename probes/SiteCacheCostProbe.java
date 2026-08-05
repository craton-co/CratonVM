import java.util.Arrays;

/**
 * Prices the two mechanisms the interpreter's resolved constant pool removes,
 * without needing a Tomcat build on the box.
 *
 * This is NOT the annotation-scan number — that one has to be taken on the
 * Azure host against real BCEL. This probe measures the two code paths the
 * site caches sit on, in the shape the scan drives them:
 *
 *   FIELD  — `getfield`/`putfield` across MANY distinct call sites against
 *            freshly allocated objects, which is what a class-file parser
 *            building one object per constant-pool entry actually does. Every
 *            one of those accesses runs `resolve_field_ref_loader_aware`, which
 *            re-derives the field-owning class from its NAME on every access
 *            including a resolution-cache hit.
 *   INVOKE — calls into registered natives with mixed signatures, which is what
 *            reaches `pop_coerced_invoke_args_*` and therefore
 *            `resolve_method_ref` on the inline-cache HIT path.
 *   MIXED  — both interleaved, the realistic shape.
 *
 * Run it under `--nojit`: with the JIT on, a compiled body uses its own field
 * access and never touches the interpreter path this is measuring, so the
 * default mode would report a number about the compiler instead. (On the real
 * scan `--nojit` is also *faster* than the default, so this is not an
 * artificial handicap — see the Tomcat known-issue doc.)
 *
 * Reports ns/op per round; several rounds so a single warm-up artifact is
 * visible rather than averaged away.
 */
public class SiteCacheCostProbe {

    static final int ROUNDS = 5;

    /** 16 fields, so one instance offers 16 distinct field-ref sites. */
    static final class Node {
        int a0, a1, a2, a3, a4, a5, a6, a7;
        long b0, b1, b2, b3;
        Object r0, r1;
        boolean f0;
        byte g0;

        Node(int seed) {
            a0 = seed; a1 = seed + 1; a2 = seed + 2; a3 = seed + 3;
            a4 = seed + 4; a5 = seed + 5; a6 = seed + 6; a7 = seed + 7;
            b0 = seed; b1 = seed + 1; b2 = seed + 2; b3 = seed + 3;
            r0 = null; r1 = null;
            f0 = (seed & 1) == 0;
            g0 = (byte) seed;
        }

        int sumInts() {
            return a0 + a1 + a2 + a3 + a4 + a5 + a6 + a7;
        }

        long sumLongs() {
            return b0 + b1 + b2 + b3;
        }

        /** A second, independent set of constant-pool entries for the same fields. */
        int weightedInts() {
            return a0 * 1 + a1 * 2 + a2 * 3 + a3 * 4 + a4 * 5 + a5 * 6 + a6 * 7 + a7 * 8;
        }

        void bump() {
            a0++; a1++; a2++; a3++; a4++; a5++; a6++; a7++;
            b0++; b1++; b2++; b3++;
        }
    }

    static long fieldWork(int iters) {
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            Node n = new Node(i);
            n.bump();
            acc += n.sumInts();
            acc += n.sumLongs();
            acc += n.weightedInts();
            acc += n.f0 ? 1 : 0;
            acc += n.g0;
            n.r0 = n;
            n.r1 = n.r0;
            acc += (n.r1 == n) ? 1 : 0;
        }
        return acc;
    }

    static long invokeWork(int iters) {
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += Math.abs(-i);
            acc += Math.abs(-(long) i);
            acc += (long) Math.abs(-(float) i);
            acc += (long) Math.abs(-(double) i);
            acc += Math.max(i, 0);
            acc += Math.min((long) i, (long) i);
            acc += Long.bitCount(i);
            acc += Integer.bitCount(i);
            acc += Long.numberOfTrailingZeros(1L << (i % 63));
            acc += Double.doubleToRawLongBits(1.0) >>> 60;
            acc += Float.floatToRawIntBits(1.0f) >>> 28;
            acc += Long.compare(i, 0);
        }
        return acc;
    }

    static long mixedWork(int iters) {
        long acc = 0;
        int[] src = new int[8];
        for (int i = 0; i < iters; i++) {
            Node n = new Node(i);
            acc += n.sumInts();
            acc += Math.abs(-(long) n.a0);
            n.bump();
            acc += n.weightedInts();
            acc += Long.bitCount(n.b0);
            int[] dst = new int[8];
            System.arraycopy(src, 0, dst, 0, 8);
            acc += dst[0] + n.sumLongs();
            acc += Math.max(n.a7, 0);
        }
        return acc;
    }

    static void bench(String name, int iters, java.util.function.IntToLongFunction body) {
        // Warm-up round, reported like the others rather than discarded: a
        // steady-state claim you cannot see the warm-up behind is not checkable.
        for (int r = 0; r < ROUNDS; r++) {
            long t0 = System.nanoTime();
            long sink = body.applyAsLong(iters);
            long t1 = System.nanoTime();
            double ns = (double) (t1 - t0) / iters;
            System.out.printf("%-8s round%d %9.1f ns/op  (sink=%d)%n", name, r, ns, sink);
        }
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        bench("field", iters, SiteCacheCostProbe::fieldWork);
        bench("invoke", iters, SiteCacheCostProbe::invokeWork);
        bench("mixed", iters, SiteCacheCostProbe::mixedWork);
        System.out.println("SITECACHE_PROBE_DONE " + Arrays.toString(args));
    }
}
