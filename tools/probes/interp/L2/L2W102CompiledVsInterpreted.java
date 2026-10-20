// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L2 (hunt): Java-visible answers of one
// method on its FIRST call (interpreted) and after it is warmed past every JIT
// threshold (single-pass, then optimizing), for the shapes where compiled code
// computes or phrases the answer itself: implicit-exception texts (division,
// bounds, null, cast, store, negative size), shifts, division overflow,
// floating-point conversions and comparisons, string switch, instanceof /
// checkcast of arrays, narrowing.
//
// Each row prints `name cold=<answer> hot=<answer>`; an answer is the value, or
// `<exception class>: <message>`. HotSpot 25 prints cold == hot on every row
// and these exact texts; diff the whole output against `java`. Run by default,
// with `--nojit` (cold == hot trivially; checks the interpreter's texts), with
// `CRATONVM_BG_COMPILE=0`, in both `--jdk-only` and `--compatible`. Compile
// with plain `javac` (no `-g`) so JEP 358 names locals `<localN>` on both VMs.
//
// A row with cold != hot is a compiled-code divergence: the file to read is
// `vm/src/jit/helpers.rs` (texts), or the backend (`jit/src/x64`, values).
public class L2W102CompiledVsInterpreted {
    static final int WARM = 30_000;

    static int[] NULL_INTS = null;
    static Holder NULL_HOLDER = null;
    static Object[] SOME_STRINGS = new String[2];
    static Object A_STRING = "s";
    static Object STRING_ARRAY = new String[1];
    static Object INT_ARRAY = new int[1];
    static Object STRING_2D = new String[1][1];
    static volatile int ZERO = 0;
    static volatile int MINUS_ONE = -1;
    static volatile long LZERO = 0L;
    static volatile long LMINUS_ONE = -1L;

    static class Holder {
        int x;
        int[] arr;

        int get() {
            return x;
        }
    }

    interface Shape {
        Object run(int mode) throws Throwable;
    }

    static String answer(Shape s, int mode) {
        try {
            Object v = s.run(mode);
            return String.valueOf(v);
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static void row(String name, Shape s) {
        String cold = answer(s, 1);
        for (int i = 0; i < WARM; i++) {
            answer(s, 0);
        }
        String hot = answer(s, 1);
        System.out.println(name + " cold=" + cold + " hot=" + hot);
    }

    // ---- division ----
    static Object idivZero(int mode) {
        int d = mode == 1 ? ZERO : 3;
        return 10 / d;
    }

    static Object lremZero(int mode) {
        long d = mode == 1 ? LZERO : 3L;
        return 10L % d;
    }

    static Object idivOverflow(int mode) {
        int d = mode == 1 ? MINUS_ONE : 3;
        int n = Integer.MIN_VALUE;
        return (n / d) + "," + (n % d);
    }

    static Object ldivOverflow(int mode) {
        long d = mode == 1 ? LMINUS_ONE : 3L;
        long n = Long.MIN_VALUE;
        return (n / d) + "," + (n % d);
    }

    // ---- bounds ----
    static Object aioobeIntRead(int mode) {
        int[] a = new int[3];
        return a[mode == 1 ? 3 : 1];
    }

    static Object aioobeNegativeWrite(int mode) {
        long[] a = new long[3];
        a[mode == 1 ? -1 : 1] = 5L;
        return a[1];
    }

    static Object aioobeByteRead(int mode) {
        byte[] a = new byte[2];
        return a[mode == 1 ? 7 : 0];
    }

    static Object aioobeCharWrite(int mode) {
        char[] a = new char[0];
        if (mode == 1) {
            a[0] = 'x';
        }
        return a.length;
    }

    static Object aioobeObjectRead(int mode) {
        String[] a = new String[1];
        return a[mode == 1 ? Integer.MAX_VALUE : 0];
    }

    // ---- null ----
    static Object npeField(int mode) {
        Holder h = mode == 1 ? NULL_HOLDER : new Holder();
        return h.x;
    }

    static Object npeArrayLoad(int mode) {
        int[] a = mode == 1 ? NULL_INTS : new int[1];
        return a[0];
    }

    static Object npeArrayLength(int mode) {
        int[] a = mode == 1 ? NULL_INTS : new int[1];
        return a.length;
    }

    static Object npeInvoke(int mode) {
        Holder h = mode == 1 ? NULL_HOLDER : new Holder();
        return h.get();
    }

    static Object npeNestedField(int mode) {
        Holder h = new Holder();
        if (mode == 0) {
            h.arr = new int[1];
        }
        return h.arr[0];
    }

    static Object npeThrowNull(int mode) throws Throwable {
        RuntimeException e = mode == 1 ? null : new RuntimeException("r");
        if (mode == 1) {
            throw e;
        }
        return e.getMessage();
    }

    static Object npeMonitor(int mode) {
        Object o = mode == 1 ? null : new Object();
        synchronized (o) {
            return 1;
        }
    }

    // ---- casts and type tests ----
    static Object cceObjectToInteger(int mode) {
        Object o = mode == 1 ? A_STRING : (Object) 7;
        return (Integer) o;
    }

    static Object cceArrayToArray(int mode) {
        Object o = mode == 1 ? STRING_ARRAY : (Object) new Integer[0];
        return ((Integer[]) o).length;
    }

    static Object ccePrimitiveArray(int mode) {
        Object o = mode == 1 ? INT_ARRAY : (Object) new Object[0];
        return ((Object[]) o).length;
    }

    static Object cceArrayToClass(int mode) {
        Object o = mode == 1 ? STRING_ARRAY : (Object) "x";
        return ((String) o).length();
    }

    static Object instanceofArrays(int mode) {
        Object o = mode == 1 ? STRING_ARRAY : STRING_2D;
        Object m = mode == 1 ? STRING_2D : STRING_ARRAY;
        Object p = INT_ARRAY;
        return (o instanceof Object[]) + "," + (o instanceof CharSequence[]) + ","
                + (o instanceof Comparable[]) + "," + (o instanceof Cloneable) + ","
                + (o instanceof java.io.Serializable) + "," + (o instanceof Object[][]) + ","
                + (m instanceof Object[][]) + "," + (m instanceof Cloneable[]) + ","
                + (p instanceof Object) + "," + (p instanceof Object[]) + ","
                + (p instanceof long[]) + "," + (p instanceof int[]);
    }

    static Object checkcastNullPasses(int mode) {
        Object o = mode == 1 ? null : "x";
        String s = (String) o;
        Integer[] a = (Integer[]) (mode == 1 ? null : new Integer[0]);
        return s + "," + a;
    }

    // ---- stores and sizes ----
    static Object arrayStore(int mode) {
        Object[] a = SOME_STRINGS;
        a[0] = mode == 1 ? (Object) Integer.valueOf(1) : "ok";
        return a[0];
    }

    static Object arrayStoreArray(int mode) {
        Object[] a = new Number[1];
        a[0] = mode == 1 ? (Object) new int[0] : (Object) 2;
        return a.length;
    }

    static Object negativeNewArray(int mode) {
        int n = mode == 1 ? -3 : 3;
        return new int[n].length;
    }

    static Object negativeANewArray(int mode) {
        int n = mode == 1 ? -4 : 3;
        return new String[n].length;
    }

    static Object negativeMultiArray(int mode) {
        int n = mode == 1 ? -2 : 2;
        return new int[3][n].length;
    }

    static Object arraycopyMismatch(int mode) {
        Object src = mode == 1 ? (Object) new String[2] : (Object) new int[2];
        int[] dst = new int[2];
        System.arraycopy(src, 0, dst, 0, 2);
        return dst.length;
    }

    static Object arraycopyBounds(int mode) {
        int[] src = new int[4];
        int[] dst = new int[2];
        System.arraycopy(src, mode == 1 ? 3 : 0, dst, 0, 2);
        return dst.length;
    }

    // ---- shifts, conversions, comparisons ----
    static Object shifts(int mode) {
        long x = mode == 1 ? 0x8000_0000_0000_0001L : 3L;
        int y = mode == 1 ? 0x8000_0001 : 3;
        int k = mode == 1 ? 65 : 1;
        int m = mode == 1 ? -1 : 2;
        return (x << k) + "," + (x >> m) + "," + (x >>> (k + 5)) + "," + (y << (k - 32)) + ","
                + (y >> m) + "," + (y >>> 33) + "," + (y << 32);
    }

    static Object conversions(int mode) {
        float f = mode == 1 ? Float.NaN : 1.5f;
        double d = mode == 1 ? Double.POSITIVE_INFINITY : 2.5;
        double big = mode == 1 ? 1e20 : 3.0;
        double neg = mode == 1 ? Double.NEGATIVE_INFINITY : -1.0;
        return ((int) f) + "," + ((long) f) + "," + ((int) d) + "," + ((long) d) + ","
                + ((int) big) + "," + ((long) big) + "," + ((int) neg) + "," + ((long) neg) + ","
                + ((int) (char) big) + "," + ((short) (int) big) + "," + ((byte) 300) + ","
                + ((float) (mode == 1 ? 16_777_217 : 1)) + "," + ((double) (mode == 1 ? Long.MAX_VALUE : 1L));
    }

    static Object nanCompares(int mode) {
        double a = mode == 1 ? Double.NaN : 1.0;
        float b = mode == 1 ? Float.NaN : 1.0f;
        return (a < 1.0) + "," + (a > 1.0) + "," + (a == a) + "," + (a != a) + ","
                + (b <= 1f) + "," + (b >= 1f) + "," + Double.compare(a, 1.0) + ","
                + Float.compare(b, b) + "," + Math.max(a, 1.0) + "," + Math.min(-0.0, 0.0) + ","
                + (0.0 == -0.0) + "," + Double.valueOf(0.0).equals(-0.0);
    }

    static Object mathEdges(int mode) {
        int a = mode == 1 ? Integer.MIN_VALUE : -5;
        long b = mode == 1 ? Long.MIN_VALUE : -5L;
        return Math.abs(a) + "," + Math.abs(b) + "," + Math.floorMod(a, 3) + ","
                + Math.floorDiv(b, -1L) + "," + (-a) + "," + (a - 1) + "," + Integer.reverse(a) + ","
                + Long.numberOfLeadingZeros(b >>> 1) + "," + Integer.bitCount(a - 1);
    }

    static Object exactOverflow(int mode) {
        int a = mode == 1 ? Integer.MAX_VALUE : 1;
        return Math.addExact(a, 1);
    }

    static Object stringSwitch(int mode) {
        // "Aa" and "BB" share a hash code; so do "AaAa" and "BBBB".
        String s = mode == 1 ? "BB" : "Aa";
        String t = mode == 1 ? "AaBB" : "BBBB";
        int r;
        switch (s) {
            case "Aa": r = 1; break;
            case "BB": r = 2; break;
            default: r = 0;
        }
        int u;
        switch (t) {
            case "AaAa": u = 10; break;
            case "BBBB": u = 20; break;
            case "AaBB": u = 30; break;
            default: u = 0;
        }
        return r + u;
    }

    static Object narrowing(int mode) {
        int v = mode == 1 ? 0x1_8F_FF : 5;
        byte b = (byte) v;
        short s = (short) v;
        char c = (char) v;
        b += 200;
        s *= 3;
        c -= 1;
        return b + "," + s + "," + (int) c + "," + (byte) (b << 1) + "," + (int) (char) -1;
    }

    static Object longCompare(int mode) {
        long a = mode == 1 ? Long.MIN_VALUE : 1L;
        long b = mode == 1 ? Long.MAX_VALUE : 2L;
        return Long.compare(a, b) + "," + (a < b) + "," + Long.compareUnsigned(a, b) + ","
                + Long.divideUnsigned(a, 3) + "," + Long.remainderUnsigned(a, 3) + ","
                + Integer.toUnsignedString((int) a - 1);
    }

    public static void main(String[] args) {
        row("idiv-zero", L2W102CompiledVsInterpreted::idivZero);
        row("lrem-zero", L2W102CompiledVsInterpreted::lremZero);
        row("idiv-overflow", L2W102CompiledVsInterpreted::idivOverflow);
        row("ldiv-overflow", L2W102CompiledVsInterpreted::ldivOverflow);
        row("aioobe-int-read", L2W102CompiledVsInterpreted::aioobeIntRead);
        row("aioobe-negative-write", L2W102CompiledVsInterpreted::aioobeNegativeWrite);
        row("aioobe-byte-read", L2W102CompiledVsInterpreted::aioobeByteRead);
        row("aioobe-char-write", L2W102CompiledVsInterpreted::aioobeCharWrite);
        row("aioobe-object-read", L2W102CompiledVsInterpreted::aioobeObjectRead);
        row("npe-field", L2W102CompiledVsInterpreted::npeField);
        row("npe-array-load", L2W102CompiledVsInterpreted::npeArrayLoad);
        row("npe-array-length", L2W102CompiledVsInterpreted::npeArrayLength);
        row("npe-invoke", L2W102CompiledVsInterpreted::npeInvoke);
        row("npe-nested-field", L2W102CompiledVsInterpreted::npeNestedField);
        row("npe-throw-null", L2W102CompiledVsInterpreted::npeThrowNull);
        row("npe-monitor", L2W102CompiledVsInterpreted::npeMonitor);
        row("cce-object-to-integer", L2W102CompiledVsInterpreted::cceObjectToInteger);
        row("cce-array-to-array", L2W102CompiledVsInterpreted::cceArrayToArray);
        row("cce-primitive-array", L2W102CompiledVsInterpreted::ccePrimitiveArray);
        row("cce-array-to-class", L2W102CompiledVsInterpreted::cceArrayToClass);
        row("instanceof-arrays", L2W102CompiledVsInterpreted::instanceofArrays);
        row("checkcast-null", L2W102CompiledVsInterpreted::checkcastNullPasses);
        row("array-store", L2W102CompiledVsInterpreted::arrayStore);
        row("array-store-array", L2W102CompiledVsInterpreted::arrayStoreArray);
        row("negative-newarray", L2W102CompiledVsInterpreted::negativeNewArray);
        row("negative-anewarray", L2W102CompiledVsInterpreted::negativeANewArray);
        row("negative-multianewarray", L2W102CompiledVsInterpreted::negativeMultiArray);
        row("arraycopy-mismatch", L2W102CompiledVsInterpreted::arraycopyMismatch);
        row("arraycopy-bounds", L2W102CompiledVsInterpreted::arraycopyBounds);
        row("shifts", L2W102CompiledVsInterpreted::shifts);
        row("conversions", L2W102CompiledVsInterpreted::conversions);
        row("nan-compares", L2W102CompiledVsInterpreted::nanCompares);
        row("math-edges", L2W102CompiledVsInterpreted::mathEdges);
        row("exact-overflow", L2W102CompiledVsInterpreted::exactOverflow);
        row("string-switch", L2W102CompiledVsInterpreted::stringSwitch);
        row("narrowing", L2W102CompiledVsInterpreted::narrowing);
        row("long-compare", L2W102CompiledVsInterpreted::longCompare);
    }
}
