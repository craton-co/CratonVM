import java.math.BigDecimal;
import java.math.BigInteger;
import java.math.MathContext;
import java.math.RoundingMode;

/**
 * Crossing-price rows for the java.math Intrinsics that may lose to the compiled
 * JDK bytecode (docs/known-issues/jdk-only/r17-java-math-intrinsics-...). One line per
 * row: {@code ROW name ns_per_op digest}. The digest must be equal across arms and
 * HotSpot; the ns column is the measurement. Usage: R17BignumOps [iters] [rounds].
 */
public class R17BignumOps {
    static long sink;

    interface Body { long run(int n); }

    static void row(String name, int iters, int rounds, Body b) {
        long dig = 0;
        for (int w = 0; w < 3; w++) dig = b.run(iters); // warm-up (compiles, OSR-free)
        long best = Long.MAX_VALUE;
        for (int r = 0; r < rounds; r++) {
            long t = System.nanoTime();
            dig = b.run(iters);
            long d = System.nanoTime() - t;
            if (d < best) best = d;
        }
        sink += dig;
        System.out.println("ROW " + name + " " + (best * 10 / iters) / 10.0 + " " + dig);
    }

    public static void main(String[] a) {
        final int iters = a.length > 0 ? Integer.parseInt(a[0]) : 200_000;
        final int rounds = a.length > 1 ? Integer.parseInt(a[1]) : 7;
        final BigDecimal[] bd = new BigDecimal[64];
        final BigInteger[] bi = new BigInteger[64];
        final BigDecimal[] wide = new BigDecimal[64];
        for (int i = 0; i < 64; i++) {
            bd[i] = new BigDecimal(BigInteger.valueOf(i * 7919L - 100000), i % 9);
            bi[i] = BigInteger.valueOf(i * 104729L - 3000000L).multiply(BigInteger.valueOf(1L << 40)).add(BigInteger.valueOf(i));
            wide[i] = new BigDecimal(bi[i].pow(3), 5 + i % 7);
        }
        row("bd-scale-call", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bd[i & 63].scale(); return h; });
        row("bd-signum-call", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bd[i & 63].signum(); return h; });
        row("bi-signum-call", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bi[i & 63].signum(); return h; });
        row("compare-compact", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bd[i & 63].compareTo(bd[(i + 5) & 63]); return h; });
        row("compare-wide", iters / 4, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += wide[i & 63].compareTo(wide[(i + 5) & 63]); return h; });
        row("bi-equals", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bi[i & 63].equals(bi[(i + 1) & 63]) ? 1 : 0; return h; });
        row("bd-add", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bd[i & 63].add(bd[(i + 3) & 63]).scale(); return h; });
        row("bd-subtract", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bd[i & 63].subtract(bd[(i + 3) & 63]).signum(); return h; });
        row("bd-multiply", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bd[i & 63].multiply(bd[(i + 3) & 63]).scale(); return h; });
        row("bd-multiply-wide", iters / 4, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += wide[i & 63].multiply(wide[(i + 3) & 63]).precision(); return h; });
        row("bd-negate", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bd[i & 63].negate().signum(); return h; });
        row("bd-valueof-long", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += BigDecimal.valueOf(i * 31L).scale(); return h; });
        row("bi-valueof-long", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += BigInteger.valueOf(i * 31L).signum(); return h; });
        row("bi-negate", iters, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bi[i & 63].negate().signum(); return h; });
        row("bd-tostring", iters / 4, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bd[i & 63].add(BigDecimal.valueOf(i)).toString().length(); return h; });
        row("bd-toplain", iters / 4, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bd[i & 63].add(BigDecimal.valueOf(i)).toPlainString().length(); return h; });
        row("bi-tostring", iters / 4, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bi[i & 63].add(BigInteger.valueOf(i)).toString().length(); return h; });
        row("bd-init-double", iters / 4, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += new BigDecimal(i * 0.37 + 0.1).precision(); return h; });
        row("fma", iters / 4, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += bd[i & 63].multiply(bd[(i + 1) & 63]).add(bd[(i + 2) & 63]).scale(); return h; });
        final MathContext mc = new MathContext(20, RoundingMode.HALF_EVEN);
        row("round-wide", iters / 8, rounds, n -> { long h = 0; for (int i = 0; i < n; i++) h += wide[i & 63].round(mc).scale(); return h; });
        if (sink == 42) System.out.println("unreachable");
    }
}
