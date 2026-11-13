// Interpreter round i1, lane L1 — JVMS edge cases of the raw-bytecode fast path
// (and the decoded-path opcodes it falls back to).
//
// Run with the interpreter only (CratonVM `--nojit`, HotSpot `-Xint`) and diff
// stdout line by line against HotSpot 25: every line is deterministic and must
// match exactly. What each section exercises:
//
//   div      idiv/irem/ldiv/lrem incl. MIN / -1, MIN % -1 and "/ by zero"
//   shift    ishl/ishr/iushr/lshl/lshr/lushr distance masking (5 / 6 bits)
//   fcmp     fcmpl/fcmpg/dcmpl/dcmpg with NaN (javac picks l vs g per operator)
//   conv     f2i/f2l/d2i/d2l saturation and NaN -> 0; i2b/i2c/i2s; l2i
//   nanbits  NaN payloads that collide with CratonVM's NaN-box tag space,
//            carried through dload_0..3 / dload n, dstore, double[] and returns
//   shuffle  dup / dup_x1 / swap-shaped expressions
//   switch   tableswitch / lookupswitch at the range edges and MIN/MAX keys
//   wide     wide iinc (constant outside -128..127)
//   multi    multianewarray: zero dims, partial dims, negative dims (the
//            OUTERMOST negative is reported), class names
public class L1FastPathEdges {
    static int ZERO = 0;
    static int MINUS_ONE = -1;

    static void div() {
        int imin = Integer.MIN_VALUE;
        long lmin = Long.MIN_VALUE;
        System.out.println("div idiv " + (imin / MINUS_ONE) + " irem " + (imin % MINUS_ONE));
        System.out.println("div ldiv " + (lmin / MINUS_ONE) + " lrem " + (lmin % MINUS_ONE));
        int m7 = -7, p7 = 7, p3 = 3, m3 = -3, p2 = 2;
        System.out.println("div irem-signs " + (m7 % p3) + " " + (p7 % m3) + " " + (m7 / p2));
        try {
            System.out.println(5 / ZERO);
        } catch (ArithmeticException e) {
            System.out.println("div idiv0 " + e.getMessage());
        }
        try {
            System.out.println(5L % ZERO);
        } catch (ArithmeticException e) {
            System.out.println("div lrem0 " + e.getMessage());
        }
    }

    static void shift() {
        int s33 = 33, s65 = 65, sneg = -1;
        System.out.println("shift i " + (1 << s33) + " " + (-8 >> s33) + " " + (-1 >>> s33)
                + " " + (1 << sneg));
        System.out.println("shift l " + (1L << s65) + " " + (-8L >> s65) + " " + (-1L >>> s65)
                + " " + (1L << sneg));
    }

    static String cmp(float a, float b) {
        return "" + (a < b) + (a <= b) + (a > b) + (a >= b) + (a == b) + (a != b);
    }

    static String cmp(double a, double b) {
        return "" + (a < b) + (a <= b) + (a > b) + (a >= b) + (a == b) + (a != b);
    }

    static void fcmp() {
        float fn = Float.NaN;
        double dn = Double.NaN;
        System.out.println("fcmp f " + cmp(fn, 1f) + " " + cmp(1f, fn) + " " + cmp(-0f, 0f));
        System.out.println("fcmp d " + cmp(dn, 1d) + " " + cmp(1d, dn) + " " + cmp(-0d, 0d));
        System.out.println("fcmp compare " + Float.compare(fn, 1f) + " " + Double.compare(-0d, 0d));
    }

    static void conv() {
        float[] fs = {Float.NaN, 1e10f, -1e10f, -0.9f, 2.5f, Float.POSITIVE_INFINITY};
        double[] ds = {Double.NaN, 1e300, -1e300, -0.9, 2147483647.9, Double.NEGATIVE_INFINITY};
        StringBuilder sb = new StringBuilder("conv");
        for (float f : fs) sb.append(' ').append((int) f).append('/').append((long) f);
        for (double d : ds) sb.append(' ').append((int) d).append('/').append((long) d);
        System.out.println(sb);
        int[] is = {200, -129, 70000, -1, 0x12345678};
        sb = new StringBuilder("conv narrow");
        for (int i : is) sb.append(' ').append((byte) i).append('/').append((int) (char) i)
                .append('/').append((short) i);
        System.out.println(sb);
        long big = 0x1_2345_6789L;
        System.out.println("conv l2i " + (int) big + " " + (int) -big + " " + (float) Long.MAX_VALUE);
    }

    // Four double parameters: `b`, `c` land in locals 2..5, so the loads below
    // use dload_2 and dload 4 (explicit index) from the same caller values.
    static long viaShortForm(double a, double b) {
        return Double.doubleToRawLongBits(b);
    }

    static long viaExplicitIndex(double a, double b, double c) {
        return Double.doubleToRawLongBits(c);
    }

    static double passThrough(double d) {
        double local = d;
        return local;
    }

    static void nanbits() {
        long[] patterns = {
            0xFFFC000000000005L, // tag space: SUB_INT
            0xFFFD000000001008L, // tag space: SUB_OBJECT-shaped
            0xFFFD800000000000L, // tag space: SUB_NULL-shaped
            0x7FF8000000000001L, // plain quiet NaN with payload
            0xFFFF000000000000L,
        };
        StringBuilder sb = new StringBuilder("nanbits");
        double[] arr = new double[patterns.length];
        for (int i = 0; i < patterns.length; i++) {
            double d = Double.longBitsToDouble(patterns[i]);
            arr[i] = d;
            boolean ok = viaShortForm(0, d) == patterns[i]
                    && viaExplicitIndex(0, 0, d) == patterns[i]
                    && Double.doubleToRawLongBits(passThrough(d)) == patterns[i]
                    && Double.doubleToRawLongBits(arr[i]) == patterns[i];
            sb.append(' ').append(ok ? "ok" : Long.toHexString(Double.doubleToRawLongBits(passThrough(d))));
        }
        System.out.println(sb);
    }

    static int field;

    static void shuffle() {
        int a, b, c;
        a = b = c = 7;
        int t = (a = a + 1) + a;
        field = 3;
        int u = field++ + field;
        long[] la = {1, 2};
        long lv = la[0] = la[1] = 40L;
        System.out.println("shuffle " + a + " " + b + " " + c + " " + t + " " + u + " " + lv
                + " " + la[0] + " " + la[1]);
    }

    static int table(int k) {
        switch (k) {
            case -2: return 10;
            case -1: return 11;
            case 0: return 12;
            case 1: return 13;
            case 2: return 14;
            default: return 99;
        }
    }

    static int lookup(int k) {
        switch (k) {
            case Integer.MIN_VALUE: return 1;
            case -100000: return 2;
            case 0: return 3;
            case 100000: return 4;
            case Integer.MAX_VALUE: return 5;
            default: return 0;
        }
    }

    static void sw() {
        int[] keys = {Integer.MIN_VALUE, -3, -2, -1, 0, 1, 2, 3, 100000, Integer.MAX_VALUE};
        StringBuilder sb = new StringBuilder("switch");
        for (int k : keys) sb.append(' ').append(table(k)).append('/').append(lookup(k));
        System.out.println(sb);
    }

    static void wide() {
        int i = 5;
        i += 1000;
        i -= 30000;
        i += 32767;
        i -= 32768;
        System.out.println("wide " + i);
    }

    static void multi() {
        int[][][] z = new int[2][0][3];
        String[][] s = new String[2][3];
        String[][][] partial = new String[2][2][];
        System.out.println("multi " + z.length + " " + z[1].length + " "
                + z.getClass().getName() + " " + s.getClass().getName() + " "
                + s[1].getClass().getName() + " " + (partial[1][1] == null) + " "
                + partial[1].getClass().getName());
        int neg1 = -1, neg2 = -2, four = 4, zero = 0;
        try {
            byte[][][] b = new byte[neg1][four][neg2];
            System.out.println("multi no throw " + b.length);
        } catch (NegativeArraySizeException e) {
            System.out.println("multi neg " + e.getMessage());
        }
        try {
            byte[][][] b = new byte[2][zero][neg1];
            System.out.println("multi no throw " + b.length);
        } catch (NegativeArraySizeException e) {
            System.out.println("multi neg-after-zero " + e.getMessage());
        }
    }

    public static void main(String[] args) {
        div();
        shift();
        fcmp();
        conv();
        nanbits();
        shuffle();
        sw();
        wide();
        multi();
    }
}
