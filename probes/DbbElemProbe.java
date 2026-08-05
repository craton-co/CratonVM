import java.nio.ByteBuffer;

/**
 * Per-element DirectByteBuffer accessor micro-benchmark — the isolated inner
 * loop of residual 4 of the (retired) `h2-jitban-residuals-20260726` write-up.
 *
 * `org.h2.compress.CompressLZF`'s `(ByteBuffer, ...)` overloads move exactly
 * one byte per call through four accessors, so a 64 KB page costs ~65,000
 * dispatches per pass. `LzfProbe` measures that end to end through H2; this
 * measures the four accessors alone, in nanoseconds per element, so a change to
 * the dispatch path can be attributed without a filesystem, a compressor or a
 * lock in the way.
 *
 *   java DbbElemProbe [iterations] [bufferBytes]
 *
 * Prints ns/element for each of `get(int)`, `put(int,byte)`, `get()`, `put(byte)`
 * plus a `mixed` loop shaped like `CompressLZF.expand` (relative put of an
 * absolute get). The checksum lines exist so a miscompiled or mis-modelled
 * accessor shows up as a wrong number rather than a fast wrong loop.
 */
public class DbbElemProbe {

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int cap = args.length > 1 ? Integer.parseInt(args[1]) : 65536;

        ByteBuffer bb = ByteBuffer.allocateDirect(cap);
        if (!bb.isDirect()) {
            System.out.println("FAIL DbbElemProbe: allocateDirect returned a non-direct buffer");
            System.exit(1);
        }
        for (int i = 0; i < cap; i++) {
            bb.put(i, (byte) (i * 31 + 7));
        }

        // Warm up every accessor so tier-up happens before the timed loops.
        run(bb, cap, Math.min(iterations, 200_000));
        Result r = run(bb, cap, iterations);

        System.out.printf("getAbs   %8.1f ns/elem%n", r.getAbs);
        System.out.printf("putAbs   %8.1f ns/elem%n", r.putAbs);
        System.out.printf("getRel   %8.1f ns/elem%n", r.getRel);
        System.out.printf("putRel   %8.1f ns/elem%n", r.putRel);
        System.out.printf("mixed    %8.1f ns/elem%n", r.mixed);
        System.out.printf("CHECKSUM %d %d%n", r.sumAbs, r.sumRel);

        long expectedAbs = expectedGetAbs(cap, iterations);
        if (r.sumAbs != expectedAbs) {
            System.out.printf("FAIL DbbElemProbe: getAbs checksum %d, expected %d%n",
                    r.sumAbs, expectedAbs);
            System.exit(1);
        }
        System.out.println("OK DbbElemProbe");
    }

    /** The value the absolute-read loop must produce, computed without the buffer. */
    private static long expectedGetAbs(int cap, int iterations) {
        long sum = 0;
        for (int i = 0, idx = 0; i < iterations; i++) {
            sum += (byte) (idx * 31 + 7);
            idx++;
            if (idx == cap) {
                idx = 0;
            }
        }
        return sum;
    }

    private static final class Result {
        double getAbs, putAbs, getRel, putRel, mixed;
        long sumAbs, sumRel;
    }

    private static Result run(ByteBuffer bb, int cap, int iterations) {
        Result r = new Result();
        long t0, t1;

        // --- absolute get(int) ---
        long sum = 0;
        t0 = System.nanoTime();
        for (int i = 0, idx = 0; i < iterations; i++) {
            sum += bb.get(idx);
            idx++;
            if (idx == cap) {
                idx = 0;
            }
        }
        t1 = System.nanoTime();
        r.sumAbs = sum;
        r.getAbs = (t1 - t0) / (double) iterations;

        // --- absolute put(int, byte) ---
        t0 = System.nanoTime();
        for (int i = 0, idx = 0; i < iterations; i++) {
            bb.put(idx, (byte) i);
            idx++;
            if (idx == cap) {
                idx = 0;
            }
        }
        t1 = System.nanoTime();
        r.putAbs = (t1 - t0) / (double) iterations;

        // Restore the deterministic pattern the checksum above expects.
        for (int i = 0; i < cap; i++) {
            bb.put(i, (byte) (i * 31 + 7));
        }

        // --- relative get() ---
        sum = 0;
        t0 = System.nanoTime();
        bb.position(0);
        for (int i = 0; i < iterations; i++) {
            if (bb.position() == cap) {
                bb.position(0);
            }
            sum += bb.get();
        }
        t1 = System.nanoTime();
        r.sumRel = sum;
        r.getRel = (t1 - t0) / (double) iterations;

        // --- relative put(byte) ---
        t0 = System.nanoTime();
        bb.position(0);
        for (int i = 0; i < iterations; i++) {
            if (bb.position() == cap) {
                bb.position(0);
            }
            bb.put((byte) i);
        }
        t1 = System.nanoTime();
        r.putRel = (t1 - t0) / (double) iterations;

        // --- `CompressLZF.expand`-shaped: relative put of an absolute get ---
        t0 = System.nanoTime();
        bb.position(0);
        for (int i = 0, idx = 0; i < iterations; i++) {
            if (bb.position() == cap) {
                bb.position(0);
            }
            bb.put(bb.get(idx));
            idx++;
            if (idx == cap) {
                idx = 0;
            }
        }
        t1 = System.nanoTime();
        r.mixed = (t1 - t0) / (double) iterations;

        // Leave the buffer in the pattern the caller's checksum assumes.
        for (int i = 0; i < cap; i++) {
            bb.put(i, (byte) (i * 31 + 7));
        }
        bb.position(0);
        return r;
    }
}
