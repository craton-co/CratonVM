/**
 * JIT round 19 proposal wave 14 (TAIL12-1, TAIL12-2). Fact lines must equal HotSpot 25 in both
 * modes; {@code time} lines are timings.
 *
 * <p>{@code div-door}: {@code Kern.div} is compiled (warmed with a non-zero divisor, then always
 * divides by zero) and called from {@code Door.run}, which the orchestrator keeps interpreted with
 * {@code CRATONVM_JIT_DENY=R19Tail14Rows$Door}: every call enters the compiled body through an
 * interpreter door, so only the door / first-call sinks see its zero-divisor guard exits. Arms:
 * default vs {@code CRATONVM_JIT_DIV_GUARD_RETIER_SINKS=1}; {@code CRATONVM_DBG_DEOPT=1} prints the
 * {@code division-guard re-tier} line. {@code div-callsite}: the same kernel called from a compiled
 * caller (the call-site service, re-tiered on every arm: the control).
 *
 * <p>{@code unroll-post-call}: a counted loop calls a spliced {@code helper}, which calls the
 * non-inlined {@code big} and then a CHA-devirtualised {@code Shape.v()}. On one call {@code big}
 * loads a second {@code Shape} subclass, so the loop body deopts right after the call returns (a
 * post-call exit), in whichever unrolled copy is running. Arms: default vs
 * {@code CRATONVM_JIT_IR_UNROLL_POST_CALL_PER_COPY=1}; the sum must not change.
 */
public class R19Tail14Rows {
    static final class Kern {
        static int div(int[] xs, int b) {
            int s = 0;
            for (int x : xs) {
                s += x;
            }
            return s / b;
        }
    }

    static final class Door {
        static long run(int[] xs, int n) {
            long s = 0;
            for (int i = 0; i < n; i++) {
                try {
                    s += Kern.div(xs, 0);
                } catch (ArithmeticException e) {
                    s++;
                }
            }
            return s;
        }
    }

    static final class Site {
        static long run(int[] xs, int n) {
            long s = 0;
            for (int i = 0; i < n; i++) {
                try {
                    s += Kern.div(xs, 0);
                } catch (ArithmeticException e) {
                    s++;
                }
            }
            return s;
        }
    }

    abstract static class Shape {
        abstract int v();
    }

    static final class Sq extends Shape {
        int v() {
            return 1;
        }
    }

    static final class Tri extends Shape {
        int v() {
            return 3;
        }
    }

    static int trigger = -1;
    static int loaded;

    static int big(int x) {
        int r = x;
        switch (x & 15) {
            case 0: r += 3; break;
            case 1: r ^= 5; break;
            case 2: r *= 7; break;
            case 3: r -= 11; break;
            case 4: r += x >> 2; break;
            case 5: r ^= x << 3; break;
            case 6: r += 13; break;
            case 7: r -= x & 7; break;
            case 8: r *= 3; break;
            case 9: r += 17; break;
            case 10: r ^= 19; break;
            case 11: r -= 23; break;
            case 12: r += x % 5; break;
            case 13: r ^= x % 7; break;
            case 14: r *= 5; break;
            default: r += 29; break;
        }
        if (x == trigger) {
            try {
                loaded += Class.forName("R19Tail14Rows$Tri").getName().length();
            } catch (ClassNotFoundException e) {
                throw new RuntimeException(e);
            }
        }
        return r;
    }

    static int helper(Shape sh, int x) {
        return big(x) + sh.v();
    }

    static long loop(Shape[] shapes, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += helper(shapes[i & 63], i);
        }
        return s;
    }

    static long timeNs(Runnable r) {
        long t0 = System.nanoTime();
        r.run();
        return System.nanoTime() - t0;
    }

    static long sinkL;

    public static void main(String[] args) {
        int[] xs = {1, 2, 3, 4, 5, 6, 7, 8};
        long warm = 0;
        for (int i = 0; i < 20_000; i++) {
            warm += Kern.div(xs, 1 + (i & 3));
        }
        System.out.println("fact warm " + warm);

        final int calls = 20_000;
        for (int rep = 0; rep < 5; rep++) {
            long[] got = new long[1];
            long ns = timeNs(() -> got[0] = Door.run(xs, calls));
            System.out.println("fact div-door." + rep + " " + got[0]);
            System.out.println("time div-door." + rep + " " + (ns / calls) + " ns/call");
        }
        for (int rep = 0; rep < 5; rep++) {
            long[] got = new long[1];
            long ns = timeNs(() -> got[0] = Site.run(xs, calls));
            System.out.println("fact div-callsite." + rep + " " + got[0]);
            System.out.println("time div-callsite." + rep + " " + (ns / calls) + " ns/call");
        }

        Shape[] shapes = new Shape[64];
        for (int i = 0; i < 64; i++) {
            shapes[i] = new Sq();
        }
        for (int i = 0; i < 200; i++) {
            sinkL += loop(shapes, 1_000);
        }
        System.out.println("fact unroll-post-call.warm " + sinkL);
        trigger = 517;
        long s = loop(shapes, 1_000);
        System.out.println("fact unroll-post-call.trip " + s + " loaded=" + (loaded > 0));
        long after = 0;
        for (int i = 0; i < 50; i++) {
            after += loop(shapes, 1_000);
        }
        System.out.println("fact unroll-post-call.after " + after);
        System.out.println("fact done");
    }
}
