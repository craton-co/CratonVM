/**
 * Focused runtime probes for the 2026-07-26 CratonVM architecture review.
 *
 * Each mode runs in a fresh process through run-architecture-probe-20260726.sh.
 * The checksum is part of the result: performance data is invalid unless the
 * CratonVM and reference-JDK checksums agree.
 */
public final class ArchitectureRuntimeProbe20260726 {
    interface Op {
        int apply(int value);
    }

    static final class Op0 implements Op { public int apply(int v) { return v + 1; } }
    static final class Op1 implements Op { public int apply(int v) { return v * 3 + 1; } }
    static final class Op2 implements Op { public int apply(int v) { return v ^ 0x55aa55aa; } }
    static final class Op3 implements Op { public int apply(int v) { return Integer.rotateLeft(v, 3); } }
    static final class Op4 implements Op { public int apply(int v) { return v - 7; } }
    static final class Op5 implements Op { public int apply(int v) { return v * 5 - 3; } }
    static final class Op6 implements Op { public int apply(int v) { return v ^ (v >>> 7); } }
    static final class Op7 implements Op { public int apply(int v) { return Integer.rotateRight(v, 5); } }
    static final class Op8 implements Op { public int apply(int v) { return v + 11; } }
    static final class Op9 implements Op { public int apply(int v) { return v * 7 + 5; } }
    static final class Op10 implements Op { public int apply(int v) { return v ^ 0x13579bdf; } }
    static final class Op11 implements Op { public int apply(int v) { return Integer.reverseBytes(v); } }
    static final class Op12 implements Op { public int apply(int v) { return v - 13; } }
    static final class Op13 implements Op { public int apply(int v) { return v * 9 - 7; } }
    static final class Op14 implements Op { public int apply(int v) { return v ^ (v << 9); } }
    static final class Op15 implements Op { public int apply(int v) { return Integer.rotateLeft(v, 11); } }

    static final Op[] ALL_OPS = {
        new Op0(), new Op1(), new Op2(), new Op3(),
        new Op4(), new Op5(), new Op6(), new Op7(),
        new Op8(), new Op9(), new Op10(), new Op11(),
        new Op12(), new Op13(), new Op14(), new Op15()
    };

    static final class Primitive8 {
        int a, b, c, d, e, f, g, h;

        Primitive8(int v) {
            a = v;
            b = v + 1;
            c = v + 2;
            d = v + 3;
            e = v + 4;
            f = v + 5;
            g = v + 6;
            h = v + 7;
        }

        int sum() {
            return a + b + c + d + e + f + g + h;
        }
    }

    static final class FastException extends RuntimeException {
        private static final long serialVersionUID = 1L;

        @Override
        public synchronized Throwable fillInStackTrace() {
            return this;
        }
    }

    private ArchitectureRuntimeProbe20260726() {}

    static long dispatch(Op[] ops, int iterations) {
        long sum = 0;
        int mask = ops.length - 1;
        for (int i = 0; i < iterations; i++) {
            sum += ops[i & mask].apply(i);
        }
        return sum;
    }

    static long allocation(int count) {
        Primitive8[] keep = new Primitive8[count];
        long sum = 0;
        for (int i = 0; i < count; i++) {
            Primitive8 value = new Primitive8(i);
            keep[i] = value;
            sum += value.sum();
        }
        return sum + keep[count - 1].h;
    }

    static long exceptionLoop(int iterations) {
        FastException exception = new FastException();
        long caught = 0;
        for (int i = 0; i < iterations; i++) {
            try {
                throw exception;
            } catch (FastException expected) {
                caught += (i & 7) + 1;
            }
        }
        return caught;
    }

    static long monitorLoop(int iterations) {
        Object lock = new Object();
        long sum = 0;
        for (int i = 0; i < iterations; i++) {
            synchronized (lock) {
                sum += (i & 15);
            }
        }
        return sum;
    }

    static long nativeLoop(int iterations) {
        long folded = 0;
        for (int i = 0; i < iterations; i++) {
            folded ^= System.nanoTime();
        }
        return folded == 0 ? 1 : folded;
    }

    static Op[] ops(int count) {
        Op[] result = new Op[count];
        System.arraycopy(ALL_OPS, 0, result, 0, count);
        return result;
    }

    static long run(String mode, int iterations) {
        if (mode.equals("dispatch-mono")) {
            return dispatch(ops(1), iterations);
        }
        if (mode.equals("dispatch-poly4")) {
            return dispatch(ops(4), iterations);
        }
        if (mode.equals("dispatch-mega16")) {
            return dispatch(ops(16), iterations);
        }
        if (mode.equals("allocation")) {
            return allocation(iterations);
        }
        if (mode.equals("exception")) {
            return exceptionLoop(iterations);
        }
        if (mode.equals("monitor")) {
            return monitorLoop(iterations);
        }
        if (mode.equals("native")) {
            return nativeLoop(iterations);
        }
        throw new IllegalArgumentException("unknown mode: " + mode);
    }

    public static void main(String[] args) {
        if (args.length != 2) {
            throw new IllegalArgumentException("usage: MODE ITERATIONS");
        }
        String mode = args[0];
        int iterations = Integer.parseInt(args[1]);

        int warm = Math.min(iterations / 10, 200_000);
        if (mode.startsWith("dispatch")) {
            run(mode, warm);
        } else if (!mode.equals("allocation")) {
            run(mode, Math.min(warm, 20_000));
        }

        long started = System.nanoTime();
        long checksum = run(mode, iterations);
        long elapsed = System.nanoTime() - started;
        System.out.println(mode + "\t" + iterations + "\t" + elapsed + "\t" + checksum);
    }
}
