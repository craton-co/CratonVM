import java.util.function.LongSupplier;
import java.util.function.DoubleSupplier;

/*
 * Interpreter round i1, wave 22, lane L7: category-2 values whose bits collide
 * with CratonVM's NaN-box tag space, pushed by every kind of producer and then
 * moved by the shuffles that must treat them as ONE value (`dup2`, `dup2_x1`,
 * `dup2_x2`, `pop2`) — and by the ones that must not.
 *
 * A `long` in [-2^50, -1] (e.g. -1L, Long.MIN_VALUE >> 20) or a negative quiet
 * NaN with mantissa bit 50 set (0xFFFC_0000_0000_0005) is bit-identical to a
 * tagged category-1 slot; only the operand stack's kind mark says it is one
 * cat-2 value. A producer that pushes such a value without the mark makes
 * `dup2` copy two "slots" (the value and whatever is below it), which shows up
 * as a wrong sum, a wrong array element, a ClassCastException or a crash.
 *
 * Producers: constant (`ldc2_w`), local, arithmetic, conversion, static and
 * instance field, array element, interpreted return, native/intrinsic return
 * (`Double.longBitsToDouble`, `Long.reverse`), lambda return, reflection.
 * Shapes: `a[i]++` on long[] (`dup2; laload; dup2_x2`), chained assignment to
 * an instance field (`dup2_x1; putfield`), to a static (`dup2; putstatic`),
 * into an array (`dup2_x2; lastore`), and a discarded call result (`pop2`).
 *
 * Run on CratonVM with `--nojit` and without; both modes. stdout is
 * deterministic and identical on HotSpot 25 (`-Xint` or not):
 *
 *   producers 12 -1 ffffffffffffffff fffc000000000005
 *   inc -1 1 -3 -4 -9223372036854775808 -9223372036854775807
 *   chain -1 -1 -1 -1 fffc000000000005 fffc000000000005 fffc000000000005
 *   arraychain -1 -1 fffc000000000005 fffc000000000005
 *   pop2 2 2
 *   mixed 7 -1 x -1 y
 *   sum -11
 */
public class L7W22Cat2Shuffles {
    static final long NEG = -1L;
    static final long NAN_BITS = 0xFFFC_0000_0000_0005L;

    static long sLong;
    static double sDouble;
    long iLong;
    double iDouble;
    static int calls;

    static long minusOne() {
        return -1L;
    }

    static double nanBoxed() {
        return Double.longBitsToDouble(NAN_BITS);
    }

    static long counted() {
        calls++;
        return -5L;
    }

    static double countedD() {
        calls++;
        return Double.longBitsToDouble(NAN_BITS);
    }

    static String hex(double d) {
        return Long.toHexString(Double.doubleToRawLongBits(d));
    }

    public static void main(String[] args) throws Exception {
        L7W22Cat2Shuffles o = new L7W22Cat2Shuffles();
        LongSupplier ls = () -> -1L;
        DoubleSupplier ds = () -> Double.longBitsToDouble(NAN_BITS);
        long viaReflect = (Long) L7W22Cat2Shuffles.class.getDeclaredMethod("minusOne").invoke(null);
        int intMinusOne = -1;
        long[] producersL = {
            NEG,                              // ldc2_w
            minusOne(),                       // interpreted return
            Long.reverse(-1L),                // intrinsic / native return
            ls.getAsLong(),                   // lambda return
            viaReflect,                       // reflection, unboxed
            (long) intMinusOne,               // i2l
            0L - 1L + intMinusOne + 1,        // arithmetic
            Long.MIN_VALUE >> 63,             // shift
        };
        double[] producersD = {
            nanBoxed(),
            ds.getAsDouble(),
            Double.longBitsToDouble(NAN_BITS),
            -Double.longBitsToDouble(0x7FFC_0000_0000_0005L), // dneg into the tag space
        };
        long lsum = 0;
        for (long v : producersL) {
            lsum += v;
        }
        boolean dSame = true;
        for (double d : producersD) {
            dSame &= Double.doubleToRawLongBits(d) == NAN_BITS;
        }
        System.out.println("producers " + (producersL.length + producersD.length) + " " + (lsum / producersL.length)
                + " " + Long.toHexString(producersL[2]) + " " + (dSame ? Long.toHexString(NAN_BITS) : "MISMATCH"));

        // a[i]++ / a[i]-- as expressions on long[]: dup2; laload; dup2_x2; ...; lastore
        long[] a = {-1L, Long.MIN_VALUE, -3L};
        int i = 0;
        long post = a[i]++;
        long pre = ++a[i];
        int k = 2;
        long post2 = a[k]--;
        long cur2 = a[k];
        int m = 1;
        long post3 = a[m]++;
        System.out.println("inc " + post + " " + pre + " " + post2 + " " + cur2 + " " + post3 + " " + a[m]);

        // chained assignments: dup2_x1; putfield / dup2; putstatic / local stores
        long local = NEG;
        long c1 = (o.iLong = local);
        long c2 = (sLong = o.iLong);
        long c3;
        long c4 = c3 = sLong;
        double d0 = Double.longBitsToDouble(NAN_BITS);
        double d1 = (o.iDouble = d0);
        double d2 = (sDouble = o.iDouble);
        System.out.println("chain " + c1 + " " + c2 + " " + c3 + " " + c4 + " " + hex(d1) + " " + hex(d2) + " "
                + hex(sDouble));

        // into arrays as expressions: dup2_x2; lastore / dastore
        long[] la = new long[2];
        double[] da = new double[2];
        int j = 1;
        long e1 = (la[j] = minusOne());
        double e2 = (da[j] = countedD());
        System.out.println("arraychain " + e1 + " " + la[1] + " " + hex(e2) + " " + hex(da[1]));

        // discarded cat-2 results: pop2
        calls = 0;
        counted();
        countedD();
        minusOne();
        nanBoxed();
        Long.reverse(-1L);
        System.out.println("pop2 " + calls + " " + (calls + 0));

        // a cat-2 value next to cat-1 values of every kind, moved through a
        // helper that takes them all (argument passing keeps each slot's kind)
        System.out.println("mixed " + mixed(7, -1L, "x", Double.longBitsToDouble(NAN_BITS), "y"));
        System.out.println("sum " + (lsum + (long) (int) calls - 5));
    }

    static String mixed(int x, long y, String s, double d, String t) {
        long y2 = y;
        double dd = d;
        String r = x + " " + y2 + " " + s;
        return r + " " + (Double.doubleToRawLongBits(dd) == NAN_BITS ? -1 : 0) + " " + t;
    }
}
