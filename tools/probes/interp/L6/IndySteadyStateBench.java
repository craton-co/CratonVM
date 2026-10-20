/*
 * Interpreter round i1, wave 16, lane L1: the steady state of a LINKED
 * `invokedynamic` in the interpreter -- every execution after the first.
 *
 * What wave 16 should speed up (vm/src/runtime/invokedynamic.rs,
 * `execute_invokedynamic_at` and the per-thread `IndySiteCache`):
 *
 *   concat, typeSwitch, enumSwitch, record
 *       the thread's entry LENDS its `Arc<ResolvedCallSite>` to the execution
 *       instead of cloning it: no refcount write on a line every thread
 *       executing the site shares (shows most in the `*MT` rows, 4 threads);
 *   concat
 *       a `String` operand is appended to the result in place
 *       (`vm::append_java_string_units`): two heap `Vec`s fewer per operand;
 *   lambda0
 *       a zero-capture lambda's singleton key `(method key, pc)` is memoised
 *       in the entry: no Fx hash of the method name and descriptor per
 *       evaluation (the global singleton-table probe remains);
 *   lambdaCapture
 *       the captures and their pin handles are inline (`SmallVec`), not two
 *       heap `Vec`s per evaluation;
 *   typeSwitch
 *       a pure type-pattern switch no longer takes the `class_manager` read
 *       lock for the receiver's class name, which none of its labels reads;
 *   all rows
 *       the entry is tagged with the class-NAME generation, so it survives a
 *       class-loading burst (not visible in this steady-state loop).
 *   stringSwitch
 *       a classic `switch` on a String (hashCode + lookupswitch, no indy):
 *       the control row, which this stage must not move.
 *
 * Measure with `--nojit`, interleaved against the previous build; the best
 * round per row goes to stderr in ns per operation. With the JIT on the
 * loops compile and the rows measure the compiled bridges instead.
 *
 * stdout is deterministic (per-row sums and a checksum) and identical on
 * HotSpot 25; the probe runner does not diff `*Bench*` files.
 */
import java.util.function.IntSupplier;
import java.util.function.IntUnaryOperator;

public class IndySteadyStateBench {
    sealed interface Shape permits Circle, Square, Rect {}

    record Circle(int r) implements Shape {}

    record Square(int side) implements Shape {}

    record Rect(int w, int h) implements Shape {}

    enum Color {
        RED,
        GREEN,
        BLUE
    }

    static final int N = 200_000;
    static final int ROUNDS = 5;
    static final int THREADS = 4;

    static final String[] NAMES = {
        "concat", "lambda0", "lambdaCapture", "typeSwitch", "enumSwitch",
        "stringSwitch", "record", "concatMT", "recordMT"
    };

    static int concat(int n) {
        int acc = 0;
        String s = "v";
        for (int i = 0; i < n; i++) {
            String t = "k" + i + ":" + s;
            acc += t.length();
        }
        return acc;
    }

    static int nonCapturing(int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            IntSupplier f = () -> 7;
            acc += f.getAsInt();
        }
        return acc;
    }

    static int capturing(int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            final int k = i & 15;
            final int m = i & 3;
            IntUnaryOperator f = x -> x + k * m;
            acc += f.applyAsInt(1);
        }
        return acc;
    }

    static int area(Shape s) {
        return switch (s) {
            case Circle c -> 3 * c.r() * c.r();
            case Square q -> q.side() * q.side();
            case Rect r -> r.w() * r.h();
        };
    }

    static int typeSwitch(int n) {
        Shape[] shapes = {new Circle(2), new Square(3), new Rect(2, 5)};
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += area(shapes[i % 3]);
        }
        return acc;
    }

    static int colorCode(Color c) {
        return switch (c) {
            case null -> -1;
            case RED -> 1;
            case GREEN -> 2;
            case BLUE -> 3;
        };
    }

    static int enumSwitch(int n) {
        Color[] cs = {Color.RED, Color.GREEN, Color.BLUE, null};
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += colorCode(cs[i & 3]);
        }
        return acc;
    }

    static int stringCode(String s) {
        switch (s) {
            case "alpha":
                return 1;
            case "beta":
                return 2;
            case "gamma":
                return 3;
            default:
                return 0;
        }
    }

    static int stringSwitch(int n) {
        String[] ss = {"alpha", "beta", "gamma", "delta"};
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += stringCode(ss[i & 3]);
        }
        return acc;
    }

    static int records(int n) {
        Rect a = new Rect(3, 4);
        Rect b = new Rect(3, 4);
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += a.hashCode();
            if (a.equals(b)) {
                acc++;
            }
            if ((i & 63) == 0) {
                acc += a.toString().length();
            }
        }
        return acc;
    }

    interface Part {
        int run(int n);
    }

    static int threaded(Part part, int n) throws InterruptedException {
        int[] out = new int[THREADS];
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int slot = t;
            ts[t] = new Thread(() -> out[slot] = part.run(n / THREADS));
            ts[t].start();
        }
        int acc = 0;
        for (int t = 0; t < THREADS; t++) {
            ts[t].join();
            acc += out[t];
        }
        return acc;
    }

    static int run(int part, int n) throws InterruptedException {
        switch (part) {
            case 0:
                return concat(n);
            case 1:
                return nonCapturing(n);
            case 2:
                return capturing(n);
            case 3:
                return typeSwitch(n);
            case 4:
                return enumSwitch(n);
            case 5:
                return stringSwitch(n);
            case 6:
                return records(n);
            case 7:
                return threaded(IndySteadyStateBench::concat, n);
            default:
                return threaded(IndySteadyStateBench::records, n);
        }
    }

    public static void main(String[] args) throws InterruptedException {
        int parts = NAMES.length;
        long[] best = new long[parts];
        int[] sums = new int[parts];
        boolean[] stable = new boolean[parts];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
        java.util.Arrays.fill(stable, true);
        for (int round = 0; round < ROUNDS; round++) {
            for (int p = 0; p < parts; p++) {
                long t0 = System.nanoTime();
                int r = run(p, N);
                long dt = System.nanoTime() - t0;
                if (round == 0) {
                    sums[p] = r;
                } else if (sums[p] != r) {
                    stable[p] = false;
                }
                best[p] = Math.min(best[p], dt);
            }
        }
        long checksum = 0;
        for (int p = 0; p < parts; p++) {
            System.out.println(NAMES[p] + "=" + sums[p] + (stable[p] ? "" : " UNSTABLE"));
            checksum = checksum * 31 + sums[p];
        }
        System.out.println("checksum=" + checksum);
        for (int p = 0; p < parts; p++) {
            System.err.println(
                    "[IndySteadyStateBench] " + NAMES[p] + " best=" + (best[p] / N) + " ns/op");
        }
    }
}
