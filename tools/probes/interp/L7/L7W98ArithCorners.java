// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L7: JVMS 6.5 arithmetic, conversion,
// comparison, shift and switch corners. Operands come from non-final statics
// so javac folds nothing; every row is computed by the bytecode under test.
// Floating results are printed as raw bits, so NaN payloads and -0.0 show.
// `all()` runs once printed and 1000 more times compared, so
// a JIT arm sees the methods warm as well as cold; rows print once (the
// first round) and every later round must agree ("rounds agree: true").
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] L7W98ArithCorners
// The HotSpot 25 run is the reference (deterministic, no timing).

public class L7W98ArithCorners {
    static int IMIN = Integer.MIN_VALUE, IMAX = Integer.MAX_VALUE, M1 = -1, ZERO = 0, SEVEN = 7, TWO = 2;
    static long LMIN = Long.MIN_VALUE, LMAX = Long.MAX_VALUE, LM1 = -1, LZERO = 0, LSEVEN = 7, LTWO = 2;
    static float FNAN = Float.NaN, FINF = Float.POSITIVE_INFINITY, FZ = 0f, FNZ = -0f, F55 = 5.5f, F2 = 2f, F1 = 1f;
    static double DNAN = Double.NaN, DINF = Double.POSITIVE_INFINITY, DZ = 0d, DNZ = -0d, D55 = 5.5, D2 = 2, D1 = 1;
    static float FBIGI = 2147483520f, FHUGE = 1e20f, F29 = 2.9f;
    static double DBIG = 1e10, D29 = 2.9, DHUGE = 1e40, DTINY = 1e-50, D2P63 = 9.223372036854775807E18;
    static int I200 = 200, I70000 = 70000, I16777217 = 16777217, IM16 = -16;
    static int[] SHIFTS = {0, 1, 31, 32, 33, -1, 63, 64, 65, -33};

    static String fb(float f) { return "f" + Integer.toHexString(Float.floatToRawIntBits(f)); }
    static String db(double d) { return "d" + Long.toHexString(Double.doubleToRawLongBits(d)); }

    static String idiv(int a, int b) {
        try { return Integer.toString(a / b); } catch (ArithmeticException e) { return "AE(" + e.getMessage() + ")"; }
    }
    static String irem(int a, int b) {
        try { return Integer.toString(a % b); } catch (ArithmeticException e) { return "AE(" + e.getMessage() + ")"; }
    }
    static String ldiv(long a, long b) {
        try { return Long.toString(a / b); } catch (ArithmeticException e) { return "AE(" + e.getMessage() + ")"; }
    }
    static String lrem(long a, long b) {
        try { return Long.toString(a % b); } catch (ArithmeticException e) { return "AE(" + e.getMessage() + ")"; }
    }

    static String integer() {
        return "idiv " + idiv(IMIN, M1) + " " + idiv(SEVEN, -TWO) + " " + idiv(-SEVEN, TWO) + " " + idiv(SEVEN, ZERO)
            + " | irem " + irem(IMIN, M1) + " " + irem(-SEVEN, TWO) + " " + irem(SEVEN, -TWO) + " " + irem(SEVEN, ZERO)
            + " | ldiv " + ldiv(LMIN, LM1) + " " + ldiv(LSEVEN, -LTWO) + " " + ldiv(LSEVEN, LZERO)
            + " | lrem " + lrem(LMIN, LM1) + " " + lrem(-LSEVEN, LTWO) + " " + lrem(LSEVEN, LZERO)
            + " | neg " + (-IMIN) + " " + (-LMIN)
            + " | mul " + (IMAX * TWO) + " " + (LMAX * LTWO);
    }

    static String floats() {
        return "fdiv " + fb(F1 / FZ) + " " + fb(-F1 / FZ) + " " + fb(FZ / FZ) + " " + fb(F1 / FNZ)
            + " | frem " + fb(F55 % F2) + " " + fb(-F55 % F2) + " " + fb(F55 % -F2) + " " + fb(F1 % FZ)
            + " " + fb(FINF % F1) + " " + fb(F1 % FINF) + " " + fb(FNZ % F1) + " " + fb(FNAN % F1)
            + " | ddiv " + db(D1 / DZ) + " " + db(-D1 / DZ) + " " + db(DZ / DZ) + " " + db(D1 / DNZ)
            + " | drem " + db(D55 % D2) + " " + db(-D55 % D2) + " " + db(D55 % -D2) + " " + db(D1 % DZ)
            + " " + db(DINF % D1) + " " + db(D1 % DINF) + " " + db(DNZ % D1) + " " + db(DNAN % D1)
            + " | neg " + fb(-FZ) + " " + db(-DZ) + " " + fb(-FNAN) + " " + db(-DNAN);
    }

    static String conversions() {
        return "d2i " + (int) DNAN + " " + (int) DINF + " " + (int) -DINF + " " + (int) DBIG + " " + (int) -DBIG
            + " " + (int) D29 + " " + (int) -D29
            + " | d2l " + (long) DNAN + " " + (long) DINF + " " + (long) -DINF + " " + (long) DHUGE + " " + (long) D2P63
            + " " + (long) -D29
            + " | f2i " + (int) FNAN + " " + (int) FINF + " " + (int) -FINF + " " + (int) FBIGI + " " + (int) FHUGE
            + " " + (int) F29 + " " + (int) -F29
            + " | f2l " + (long) FNAN + " " + (long) FINF + " " + (long) -FINF + " " + (long) FHUGE
            + " | d2f " + fb((float) DHUGE) + " " + fb((float) -DHUGE) + " " + fb((float) DTINY) + " " + fb((float) -DTINY)
            + " " + fb((float) DNAN)
            + " | i2f " + fb((float) I16777217) + " l2f " + fb((float) LMAX) + " l2d " + db((double) (LMAX - 1))
            + " | i2b " + (byte) I200 + " i2c " + (int) (char) M1 + " i2s " + (short) I70000
            + " | i2l " + (long) IMIN + " l2i " + (int) LMAX + " " + (int) (LMAX - 0x80000000L);
    }

    static String b(boolean v) { return v ? "T" : "F"; }

    static String compares() {
        float f = FNAN, g = F1;
        double d = DNAN, e = D1;
        return "f< " + b(f < g) + " f> " + b(f > g) + " f<= " + b(f <= g) + " f>= " + b(f >= g) + " f== " + b(f == f)
            + " f!= " + b(f != f) + " !(f<g) " + b(!(f < g)) + " !(f>g) " + b(!(f > g))
            + " | d< " + b(d < e) + " d> " + b(d > e) + " d<= " + b(d <= e) + " d>= " + b(d >= e) + " d== " + b(d == d)
            + " !(d<e) " + b(!(d < e)) + " !(d>e) " + b(!(d > e))
            + " | zeros " + b(FZ == FNZ) + b(DZ == DNZ) + b(FZ < FNZ) + b(DNZ < DZ)
            + " | Float.compare " + Float.compare(FZ, FNZ) + " " + Float.compare(FNAN, FINF)
            + " | lcmp " + b(LMIN < LMAX) + b(LMAX > LMIN) + b(LMIN < LZERO) + b(LM1 > LMIN);
    }

    static String shifts() {
        StringBuilder sb = new StringBuilder("ishl");
        for (int s : SHIFTS) sb.append(' ').append(1 << s);
        sb.append(" | ishr");
        for (int s : SHIFTS) sb.append(' ').append(IM16 >> s);
        sb.append(" | iushr");
        for (int s : SHIFTS) sb.append(' ').append(IM16 >>> s);
        sb.append(" | lshl");
        for (int s : SHIFTS) sb.append(' ').append(1L << s);
        sb.append(" | lshr");
        for (int s : SHIFTS) sb.append(' ').append((long) IM16 >> s);
        sb.append(" | lushr");
        for (int s : SHIFTS) sb.append(' ').append((long) IM16 >>> s);
        return sb.toString();
    }

    static String tableLow(int x) {
        switch (x) {
            case Integer.MIN_VALUE: return "min";
            case Integer.MIN_VALUE + 1: return "min+1";
            case Integer.MIN_VALUE + 2: return "min+2";
            case Integer.MIN_VALUE + 3: return "min+3";
            default: return "dflt";
        }
    }
    static String tableHigh(int x) {
        switch (x) {
            case Integer.MAX_VALUE - 3: return "max-3";
            case Integer.MAX_VALUE - 2: return "max-2";
            case Integer.MAX_VALUE - 1: return "max-1";
            case Integer.MAX_VALUE: return "max";
            default: return "dflt";
        }
    }
    static String lookup(int x) {
        switch (x) {
            case Integer.MIN_VALUE: return "min";
            case -1: return "m1";
            case 0: return "zero";
            case 1000000: return "mil";
            case Integer.MAX_VALUE: return "max";
            default: return "dflt";
        }
    }

    static String switches() {
        int[] xs = {IMIN, IMIN + 1, IMIN + 3, IMIN + 4, M1, ZERO, 1000000, IMAX - 3, IMAX - 1, IMAX, IMAX - 4};
        StringBuilder sb = new StringBuilder("tlow");
        for (int x : xs) sb.append(' ').append(tableLow(x));
        sb.append(" | thigh");
        for (int x : xs) sb.append(' ').append(tableHigh(x));
        sb.append(" | lookup");
        for (int x : xs) sb.append(' ').append(lookup(x));
        return sb.toString();
    }

    static String iinc() {
        int a = SEVEN, b = IMAX, c = IMIN;
        a += 1000;     // wide iinc
        a -= 30000;    // wide iinc, negative
        b += 1;        // wraps
        c -= 1;        // wraps
        int d = SEVEN;
        d += 127;
        d -= 128;
        return "iinc " + a + " " + b + " " + c + " " + d;
    }

    static String[] all() {
        return new String[] {integer(), floats(), conversions(), compares(), shifts(), switches(), iinc()};
    }

    public static void main(String[] args) {
        String[] first = all();
        for (String row : first) System.out.println(row);
        boolean agree = true;
        for (int round = 0; round < 1000; round++) {
            String[] again = all();
            for (int i = 0; i < first.length; i++) {
                if (!first[i].equals(again[i])) {
                    if (agree) System.out.println("round " + round + " differs: " + again[i]);
                    agree = false;
                }
            }
        }
        System.out.println("rounds agree: " + agree);
    }
}
