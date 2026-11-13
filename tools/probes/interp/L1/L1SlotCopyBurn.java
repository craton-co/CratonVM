// Interpreter round i1, lane L1 — prices the fast-path arms this round moved
// from a `Value` decode/re-encode to raw slot copies: fload_<n>/fstore_<n>,
// dload_<n> and dup, each inside an ordinary javac (top-tested) loop.
//
// stdout: one deterministic checksum line per arm (must match HotSpot 25).
// stderr: ns per loop iteration, min over the rounds, arms interleaved in
// alternating order so host drift hits every arm equally.
//
// Measure: CratonVM `--nojit` built before and after the lane-L1 change, one
// run each, interleaved (A B A B ...), compare medians of the stderr numbers.
// HotSpot `-Xint` gives the reference column. Arguments: [n] [rounds].
public class L1SlotCopyBurn {
    // n in local 0; a, b, c in locals 1..3 -> fload_1..3 / fstore_1..3.
    static float floats(int n) {
        float a = 0f, b = 1.0001f, c = 0.5f;
        for (int i = 0; i < n; i++) {
            a = a * b + c;
            c = a - c;
            if (a > 1e6f) a = 0f;
        }
        return a + c;
    }

    // n in local 0; a in 1-2, b in 3-4 -> dload_1 / dload_3 / dstore_1.
    static double doubles(int n) {
        double a = 0d, b = 1.0000001d;
        for (int i = 0; i < n; i++) {
            a = a * b + 1d;
            if (a > 1e12d) a = 0d;
        }
        return a + b;
    }

    // `x = y = expr` compiles to `...; dup; istore y; istore x`.
    static int dups(int n) {
        int x = 0, y = 0;
        for (int i = 0; i < n; i++) {
            x = y = x + i;
        }
        return x ^ y;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 5_000_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 7;
        double tf = 1e18, td = 1e18, tu = 1e18;
        float sf = 0;
        double sd = 0;
        int su = 0;
        long t;
        for (int r = 0; r < rounds; r++) {
            if ((r & 1) == 0) {
                t = System.nanoTime(); sf = floats(n);  tf = Math.min(tf, (System.nanoTime() - t) / (double) n);
                t = System.nanoTime(); sd = doubles(n); td = Math.min(td, (System.nanoTime() - t) / (double) n);
                t = System.nanoTime(); su = dups(n);    tu = Math.min(tu, (System.nanoTime() - t) / (double) n);
            } else {
                t = System.nanoTime(); su = dups(n);    tu = Math.min(tu, (System.nanoTime() - t) / (double) n);
                t = System.nanoTime(); sd = doubles(n); td = Math.min(td, (System.nanoTime() - t) / (double) n);
                t = System.nanoTime(); sf = floats(n);  tf = Math.min(tf, (System.nanoTime() - t) / (double) n);
            }
        }
        System.out.println("floats " + Float.floatToIntBits(sf));
        System.out.println("doubles " + Double.doubleToLongBits(sd));
        System.out.println("dups " + su);
        System.err.println("ns/iter floats=" + tf + " doubles=" + td + " dups=" + tu);
    }
}
