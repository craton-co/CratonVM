import java.math.BigDecimal;

/**
 * SBR3 UNSAFECAS: the OSR defect. Run under CRATONVM_PURE_BYTECODE=1 CRATONVM_TIER_OSR_BACKEDGE=30
 * (RJdkBigNum dv.15.1 threw ArrayIndexOutOfBoundsException) and in default mode with
 * CRATONVM_TIER_OSR_BACKEDGE=10. Prints a line per kernel; the last column is the HotSpot answer.
 */
public class Sbr3UnsafecasOsr {
    static int bad;
    static void check(String what, Object got, Object want) {
        if (!String.valueOf(got).equals(String.valueOf(want))) {
            bad++;
            if (bad < 20) System.out.println("MISMATCH " + what + " got=" + got + " want=" + want);
        }
    }
    // The arithmetic of jdk.internal.math.MathUtils.flog10pow2 / flog2pow10 (what made g1() index 2137324).
    static int flog10pow2(int e) { return (int) (e * 661_971_961_083L >> 41); }
    static int flog2pow10(int e) { return (int) (e * 913_124_641_741L >> 38); }
    static long sumKernel(int n) {
        long s = 0;
        for (int e = -n; e <= n; e++) s += flog10pow2(e) * 31L + flog2pow10(e);
        return s;
    }
    static void arith() {
        long want = 0;
        long first = sumKernel(1000);
        for (int r = 0; r < 200; r++) {
            long got = sumKernel(1000);
            if (got != first) { bad++; if (bad < 20) System.out.println("MISMATCH sumKernel@" + r + " got=" + got + " want=" + first); }
        }
        System.out.println("sumKernel=" + first);
    }
    // Negative int argument through a one-call-per-invocation path (interpreter -> compiled callee).
    static int viaCall(int q) { return flog10pow2(q); }
    static void negArg() {
        int[] qs = { -52, -1, -1074, -3, 0, 5, 1000 };
        int[] want = { -16, -1, -324, -1, 0, 1, 301 };
        int badHere = 0;
        for (int r = 0; r < 4000; r++) {
            for (int i = 0; i < qs.length; i++) {
                int got = viaCall(qs[i]);
                if (got != want[i]) { badHere++; if (badHere < 5) System.out.println("MISMATCH viaCall(" + qs[i] + ")@" + r + " got=" + got + " want=" + want[i]); }
            }
        }
        bad += badHere;
    }
    public static void main(String[] a) throws Throwable {
        arith();
        negArg();
        String[] s = { "1.5", "-1.5", "1.5E-307", "9.007199254740992E14", "0.1", "12345.678", "1e23", "4.9E-324", "1.7976931348623157E308" };
        double[] want = { 1.5, -1.5, 1.5E-307, 9.007199254740992E14, 0.1, 12345.678, 1e23, 4.9E-324, 1.7976931348623157E308 };
        int rounds = Integer.getInteger("rounds", 3000);
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < s.length; i++) {
                try {
                    check("parseDouble(" + s[i] + ")@" + r, Double.parseDouble(s[i]), want[i]);
                } catch (Throwable t) {
                    bad++;
                    if (bad < 20) System.out.println("THROW parseDouble(" + s[i] + ")@" + r + " " + t);
                    if (bad == 1) t.printStackTrace(System.out);
                }
            }
            try {
                check("bd.doubleValue@" + r, new BigDecimal(java.math.BigInteger.valueOf(15), 1).doubleValue(), 1.5);
                check("bd.doubleValue308@" + r, new BigDecimal(java.math.BigInteger.valueOf(15), 308).doubleValue(), 1.5E-307);
                check("bd.floatValue@" + r, new BigDecimal(java.math.BigInteger.valueOf(15), 1).floatValue(), 1.5f);
                check("Double.toString@" + r, Double.toString(1.5E-307) + Double.toString(9.007199254740992E14), "1.5E-3079.007199254740992E14");
                check("Math.abs@" + r, Math.abs(-15L) + Math.abs(-1.5), 16.5);
            } catch (Throwable t) {
                bad++;
                if (bad < 20) System.out.println("THROW bd@" + r + " " + t);
                if (bad == 1) t.printStackTrace(System.out);
            }
        }
        System.out.println("rounds=" + rounds + " bad=" + bad);
    }
}
