// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 5 (lane SPHUNT): HotSpot 25's answers at the throwing sites and
// arithmetic corner cases of the single-pass (baseline) x64 tier.
//
// Every throwing method is first WARMED with inputs that do not throw (so the
// single-pass tier, and HotSpot's C1/C2, compile it with the throwing edge
// never taken), then called THROWS times with a throwing input. `bad` counts
// later throws whose answer differed from the first. THROWS is small on
// purpose: HotSpot's OmitStackTraceInFastThrow hands out preallocated,
// message-less exceptions once a compiled site has trapped often enough, which
// would make a long throwing loop non-deterministic on HotSpot itself. For
// the same reason every throwing ROW has its own method: C2 recompiles a
// bci that trapped even once with `too_many_traps` true, and its
// `builtin_throw` then throws the preallocated exception (message null), so
// a second row throwing at the same bci after a fresh warm-up would print
// `null` on HotSpot depending on background-compile timing.
//
// Compile with plain `javac` (no -g): the helpful-NPE lines below name the
// parameters `<parameterN>`, HotSpot's spelling when there is no
// LocalVariableTable (with -g they would name `a`, `b`, `o`, ...).
//
//   javac -d <dir> J27BaselineThrowRows.java
//   java -cp <dir> J27BaselineThrowRows > hs.txt
//   cratonvm --cp <dir> J27BaselineThrowRows | diff - hs.txt              (--real-jdk)
//   cratonvm --compatible --cp <dir> J27BaselineThrowRows | diff - hs.txt
//   also with --nojit (interpreter reference) and with the IR tier off
//   (single-pass only) if the build has a switch for that.
//
// Expected HotSpot 25 output (derived from JLS 15.17 / JVMS 6.5 / JEP 358 and
// HotSpot's SharedRuntime::generate_class_cast_message; the HotSpot run is
// the authority):
//   fact iaload-null bad=0 java.lang.NullPointerException Cannot load from int array because "<parameter1>" is null top=ialoadNull
//   fact iaload-high bad=0 java.lang.ArrayIndexOutOfBoundsException Index 5 out of bounds for length 3 top=ialoadHigh
//   fact iaload-neg bad=0 java.lang.ArrayIndexOutOfBoundsException Index -1 out of bounds for length 3 top=ialoadNeg
//   fact iaload-minint bad=0 java.lang.ArrayIndexOutOfBoundsException Index -2147483648 out of bounds for length 3 top=ialoadMin
//   fact iastore-null bad=0 java.lang.NullPointerException Cannot store to int array because "<parameter1>" is null top=iastoreNull
//   fact iastore-eq-len bad=0 java.lang.ArrayIndexOutOfBoundsException Index 3 out of bounds for length 3 top=iastoreHigh
//   fact baload-null bad=0 java.lang.NullPointerException Cannot load from byte/boolean array because "<parameter1>" is null top=baload
//   fact caload-high bad=0 java.lang.ArrayIndexOutOfBoundsException Index 0 out of bounds for length 0 top=caload
//   fact laload-null bad=0 java.lang.NullPointerException Cannot load from long array because "<parameter1>" is null top=laload
//   fact daload-high bad=0 java.lang.ArrayIndexOutOfBoundsException Index 7 out of bounds for length 2 top=daload
//   fact aaload-null bad=0 java.lang.NullPointerException Cannot load from object array because "<parameter1>" is null top=aaload
//   fact aastore-null-array bad=0 java.lang.NullPointerException Cannot store to object array because "<parameter1>" is null top=aastoreNull
//   fact aastore-high-before-type bad=0 java.lang.ArrayIndexOutOfBoundsException Index 2 out of bounds for length 1 top=aastoreHigh
//   fact aastore-type bad=0 java.lang.ArrayStoreException java.lang.Integer top=aastoreType
//   fact aastore-int-array-type bad=0 java.lang.ArrayStoreException [I top=aastoreIntArr
//   fact aastore-null-value bad=0 ok
//   fact arraylength-null bad=0 java.lang.NullPointerException Cannot read the array length because "<parameter1>" is null top=alen
//   fact getfield-null bad=0 java.lang.NullPointerException Cannot read field "v" because "<parameter1>" is null top=getf
//   fact putfield-null bad=0 java.lang.NullPointerException Cannot assign field "v" because "<parameter1>" is null top=putf
//   fact invoke-null bad=0 java.lang.NullPointerException Cannot invoke "J27BtBox.get()" because "<parameter1>" is null top=invoke
//   fact monitorenter-null bad=0 java.lang.NullPointerException Cannot enter synchronized block because "<parameter1>" is null top=sync
//   fact athrow-null bad=0 java.lang.NullPointerException Cannot throw exception because "<parameter1>" is null top=athrow
//   fact idiv-zero bad=0 java.lang.ArithmeticException / by zero top=idiv
//   fact irem-zero bad=0 java.lang.ArithmeticException / by zero top=irem
//   fact ldiv-zero bad=0 java.lang.ArithmeticException / by zero top=ldiv
//   fact lrem-zero bad=0 java.lang.ArithmeticException / by zero top=lrem
//   fact idiv-const-zero bad=0 java.lang.ArithmeticException / by zero top=idivConstZero
//   fact div-min-by-m1 -2147483648 0 -9223372036854775808 0
//   fact div-min-by-m1-const -2147483648 0 -9223372036854775808 0
//   fact div-neg-const -3 -1 3 1 -3 -1
//   fact div-pow2 -1 -3 -1 -1 -1 -3 -1 -1
//   fact newarray-neg bad=0 java.lang.NegativeArraySizeException -5 top=newInts
//   fact anewarray-neg bad=0 java.lang.NegativeArraySizeException -5 top=newStrs
//   fact multianewarray-inner-neg bad=0 java.lang.NegativeArraySizeException -3 top=newMulti
//   fact multianewarray-outer-first bad=0 java.lang.NegativeArraySizeException -1 top=newMulti3
//   fact multianewarray-zero-then-neg bad=0 java.lang.NegativeArraySizeException -1 top=newMulti3a
//   fact multianewarray-zero-outer bad=0 java.lang.NegativeArraySizeException -1 top=newMulti3b
//   fact checkcast-string bad=0 java.lang.ClassCastException class java.lang.Integer cannot be cast to class java.lang.String (java.lang.Integer and java.lang.String are in module java.base of loader 'bootstrap') top=castStr
//   fact checkcast-app bad=0 java.lang.ClassCastException class java.lang.Integer cannot be cast to class J27BtBox (java.lang.Integer is in module java.base of loader 'bootstrap'; J27BtBox is in unnamed module of loader 'app') top=castBox
//   fact checkcast-prim-array bad=0 java.lang.ClassCastException class [J cannot be cast to class [I ([J and [I are in module java.base of loader 'bootstrap') top=castInts
//   fact checkcast-null ok null
//   fact instanceof-null 0
//   fact shift 2 -1 -1 2 -1 1073741823 4294967295 -2147483648
//   fact narrow -56 65535 -25536 -1 0 -128
//   fact f2i 0 2147483647 -2147483648 0 9223372036854775807 -9223372036854775808 2147483647 -2147483648 3 -3
//   fact frem -1.5 1.5 NaN NaN -0.0 0.0
//   fact fneg -0.0 0.0 true
//   fact locals-after-aioobe 111325
//   fact locals-after-div 111325
//   fact locals-after-npe 111325
//   fact locals-after-cce 111325
//   fact poll-gettrace ok top=spin
//   fact poll-gc ok
//   fact poll-spin-stopped ok
public class J27BaselineThrowRows {
    static final int WARM = 30000;
    static final int THROWS = 3;
    static long sink;

    interface Body {
        void run();
    }

    // ---- the compiled sites ------------------------------------------------

    static int ialoadNull(int[] a, int i) {
        return a[i];
    }

    static int ialoadHigh(int[] a, int i) {
        return a[i];
    }

    static int ialoadNeg(int[] a, int i) {
        return a[i];
    }

    static int ialoadMin(int[] a, int i) {
        return a[i];
    }

    static void iastoreNull(int[] a, int i, int v) {
        a[i] = v;
    }

    static void iastoreHigh(int[] a, int i, int v) {
        a[i] = v;
    }

    static int baload(byte[] a, int i) {
        return a[i];
    }

    static int caload(char[] a, int i) {
        return a[i];
    }

    static long laload(long[] a, int i) {
        return a[i];
    }

    static double daload(double[] a, int i) {
        return a[i];
    }

    static Object aaload(Object[] a, int i) {
        return a[i];
    }

    static void aastoreNull(Object[] a, int i, Object v) {
        a[i] = v;
    }

    static void aastoreHigh(Object[] a, int i, Object v) {
        a[i] = v;
    }

    static void aastoreType(Object[] a, int i, Object v) {
        a[i] = v;
    }

    static void aastoreIntArr(Object[] a, int i, Object v) {
        a[i] = v;
    }

    static void aastoreNullVal(Object[] a, int i, Object v) {
        a[i] = v;
    }

    static int alen(int[] a) {
        return a.length;
    }

    static int getf(J27BtBox b) {
        return b.v;
    }

    static void putf(J27BtBox b, int v) {
        b.v = v;
    }

    static int invoke(J27BtBox b) {
        return b.get();
    }

    static int sync(Object o) {
        synchronized (o) {
            return 1;
        }
    }

    static void athrow(RuntimeException e) {
        throw e;
    }

    static int idiv(int a, int b) {
        return a / b;
    }

    static int irem(int a, int b) {
        return a % b;
    }

    static long ldiv(long a, long b) {
        return a / b;
    }

    static long lrem(long a, long b) {
        return a % b;
    }

    static int idivConstZero(int a, boolean go) {
        // `x / 0` with a visible zero constant: no fold may drop the throw.
        return go ? a / 0 : a;
    }

    static int idivM1(int a) {
        return a / -1;
    }

    static int iremM1(int a) {
        return a % -1;
    }

    static long ldivM1(long a) {
        return a / -1L;
    }

    static long lremM1(long a) {
        return a % -1L;
    }

    static int[] newInts(int n) {
        return new int[n];
    }

    static String[] newStrs(int n) {
        return new String[n];
    }

    static int[][] newMulti(int a, int b) {
        return new int[a][b];
    }

    static byte[][][] newMulti3(int a, int b, int c) {
        return new byte[a][b][c];
    }

    static byte[][][] newMulti3a(int a, int b, int c) {
        return new byte[a][b][c];
    }

    // HotSpot's multi_allocate still checks the lower dimensions of a
    // zero-length level: `new byte[0][-1][-1]` throws `-1`.
    static byte[][][] newMulti3b(int a, int b, int c) {
        return new byte[a][b][c];
    }

    static String castStr(Object o) {
        return (String) o;
    }

    static J27BtBox castBox(Object o) {
        return (J27BtBox) o;
    }

    static int[] castInts(Object o) {
        return (int[]) o;
    }

    static boolean isStr(Object o) {
        return o instanceof String;
    }

    // Live locals of every kind, all reassigned inside the try, read by the
    // handler: the compiled throw must hand the handler this bci's frame.
    static int localsAfterAioobe(int[] a, int i) {
        int x = 7;
        long y = 9;
        double z = 0.5;
        Object o = "s";
        try {
            x = 11;
            y = 13;
            z = 2.5;
            o = "t";
            return a[i];
        } catch (ArrayIndexOutOfBoundsException e) {
            return x * 10000 + (int) y * 100 + (int) (z * 10) + ("t".equals(o) ? 0 : 900000);
        }
    }

    static int localsAfterDiv(int d) {
        int x = 7;
        long y = 9;
        double z = 0.5;
        Object o = "s";
        try {
            x = 11;
            y = 13;
            z = 2.5;
            o = "t";
            return 100 / d;
        } catch (ArithmeticException e) {
            return x * 10000 + (int) y * 100 + (int) (z * 10) + ("t".equals(o) ? 0 : 900000);
        }
    }

    static int localsAfterNpe(J27BtBox b) {
        int x = 7;
        long y = 9;
        double z = 0.5;
        Object o = "s";
        try {
            x = 11;
            y = 13;
            z = 2.5;
            o = "t";
            return b.v;
        } catch (NullPointerException e) {
            return x * 10000 + (int) y * 100 + (int) (z * 10) + ("t".equals(o) ? 0 : 900000);
        }
    }

    static int localsAfterCce(Object p) {
        int x = 7;
        long y = 9;
        double z = 0.5;
        Object o = "s";
        try {
            x = 11;
            y = 13;
            z = 2.5;
            o = "t";
            return ((String) p).length();
        } catch (ClassCastException e) {
            return x * 10000 + (int) y * 100 + (int) (z * 10) + ("t".equals(o) ? 0 : 900000);
        }
    }

    static int shl(int a, int n) {
        return a << n;
    }

    static int ushr(int a, int n) {
        return a >>> n;
    }

    static long lshl(long a, int n) {
        return a << n;
    }

    static long lushr(long a, int n) {
        return a >>> n;
    }

    static int sar(int a, int n) {
        return a >> n;
    }

    static int i2b(int a) {
        return (byte) a;
    }

    static int i2c(int a) {
        return (char) a;
    }

    static int i2s(int a) {
        return (short) a;
    }

    static int f2i(float f) {
        return (int) f;
    }

    static long d2l(double d) {
        return (long) d;
    }

    static int d2i(double d) {
        return (int) d;
    }

    static float frem(float a, float b) {
        return a % b;
    }

    static double drem(double a, double b) {
        return a % b;
    }

    static float fneg(float a) {
        return -a;
    }

    static double dneg(double a) {
        return -a;
    }

    static int idivPow2(int a) {
        return a / 4;
    }

    static int iremPow2(int a) {
        return a % 4;
    }

    static long ldivPow2(long a) {
        return a / 4L;
    }

    static long lremPow2(long a) {
        return a % 4L;
    }

    static int idivNeg3(int a) {
        return a / -3;
    }

    static int iremNeg3(int a) {
        return a % -3;
    }

    static long ldivNeg3(long a) {
        return a / -3L;
    }

    static long lremNeg3(long a) {
        return a % -3L;
    }

    // ---- row harness -------------------------------------------------------

    static String describe(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        String top = st.length == 0 ? "<none>" : st[0].getMethodName();
        return t.getClass().getName() + " " + t.getMessage() + " top=" + top;
    }

    static String outcome(Body body) {
        try {
            body.run();
            return "no-exception";
        } catch (RuntimeException e) {
            return describe(e);
        }
    }

    static void row(String name, Runnable warm, Body thrower) {
        for (int i = 0; i < WARM; i++) {
            warm.run();
        }
        String first = outcome(thrower);
        int bad = 0;
        for (int i = 1; i < THROWS; i++) {
            if (!first.equals(outcome(thrower))) {
                bad++;
            }
        }
        System.out.println("fact " + name + " bad=" + bad + " " + first);
    }

    static void arrayRows() {
        int[] ints = {1, 2, 3};
        byte[] bytes = {1, 2};
        char[] chars = {'a'};
        long[] longs = {1L, 2L};
        double[] doubles = {1.0, 2.0};
        Object[] objs = {"a"};
        row("iaload-null", () -> sink += ialoadNull(ints, 1), () -> sink += ialoadNull(null, 1));
        row("iaload-high", () -> sink += ialoadHigh(ints, 2), () -> sink += ialoadHigh(ints, 5));
        row("iaload-neg", () -> sink += ialoadNeg(ints, 0), () -> sink += ialoadNeg(ints, -1));
        row("iaload-minint", () -> sink += ialoadMin(ints, 0), () -> sink += ialoadMin(ints, Integer.MIN_VALUE));
        row("iastore-null", () -> iastoreNull(ints, 1, 2), () -> iastoreNull(null, 1, 2));
        row("iastore-eq-len", () -> iastoreHigh(ints, 2, 3), () -> iastoreHigh(ints, 3, 3));
        row("baload-null", () -> sink += baload(bytes, 1), () -> sink += baload(null, 1));
        row("caload-high", () -> sink += caload(chars, 0), () -> sink += caload(new char[0], 0));
        row("laload-null", () -> sink += laload(longs, 1), () -> sink += laload(null, 1));
        row("daload-high", () -> sink += (long) daload(doubles, 1), () -> sink += (long) daload(doubles, 7));
        row("aaload-null", () -> sink += aaload(objs, 0).hashCode(), () -> sink += aaload(null, 0).hashCode());
        Object[] strs = new String[1];
        row("aastore-null-array", () -> aastoreNull(strs, 0, "x"), () -> aastoreNull(null, 5, Integer.valueOf(1)));
        row("aastore-high-before-type", () -> aastoreHigh(strs, 0, "x"), () -> aastoreHigh(strs, 2, Integer.valueOf(1)));
        row("aastore-type", () -> aastoreType(strs, 0, "x"), () -> aastoreType(strs, 0, Integer.valueOf(1)));
        Object[] objArr = new Object[1];
        Object[] numArr = new Number[1];
        row("aastore-int-array-type", () -> aastoreIntArr(objArr, 0, ints), () -> aastoreIntArr(numArr, 0, ints));
        String ok = "ok";
        try {
            for (int i = 0; i < WARM; i++) {
                aastoreNullVal(strs, 0, i == 0 ? "x" : null);
            }
        } catch (RuntimeException e) {
            ok = describe(e);
        }
        System.out.println("fact aastore-null-value bad=0 " + ok);
        row("arraylength-null", () -> sink += alen(ints), () -> sink += alen(null));
    }

    static void objectRows() {
        J27BtBox box = new J27BtBox();
        row("getfield-null", () -> sink += getf(box), () -> sink += getf(null));
        row("putfield-null", () -> putf(box, 3), () -> putf(null, 3));
        row("invoke-null", () -> sink += invoke(box), () -> sink += invoke(null));
        row("monitorenter-null", () -> sink += sync(box), () -> sink += sync(null));
        RuntimeException pre = new IllegalStateException("warm");
        row(
                "athrow-null",
                () -> {
                    try {
                        athrow(pre);
                    } catch (IllegalStateException e) {
                        sink++;
                    }
                },
                () -> athrow(null));
    }

    static void arithRows() {
        row("idiv-zero", () -> sink += idiv(7, 3), () -> sink += idiv(7, 0));
        row("irem-zero", () -> sink += irem(7, 3), () -> sink += irem(7, 0));
        row("ldiv-zero", () -> sink += ldiv(7L, 3L), () -> sink += ldiv(7L, 0L));
        row("lrem-zero", () -> sink += lrem(7L, 3L), () -> sink += lrem(7L, 0L));
        row("idiv-const-zero", () -> sink += idivConstZero(7, false), () -> sink += idivConstZero(7, true));
        int imin = Integer.MIN_VALUE;
        long lmin = Long.MIN_VALUE;
        for (int i = 0; i < WARM; i++) {
            sink += idiv(i | 1, -1) + irem(i, -1) + ldiv(i, -1L) + lrem(i, -1L);
            sink += idivM1(i) + iremM1(i) + ldivM1(i) + lremM1(i);
            sink += idivPow2(i) + iremPow2(i) + ldivPow2(i) + lremPow2(i);
            sink += idivNeg3(i) + iremNeg3(i) + ldivNeg3(i) + lremNeg3(i);
        }
        System.out.println("fact div-min-by-m1 " + idiv(imin, -1) + " " + irem(imin, -1) + " "
                + ldiv(lmin, -1L) + " " + lrem(lmin, -1L));
        System.out.println("fact div-min-by-m1-const " + idivM1(imin) + " " + iremM1(imin) + " "
                + ldivM1(lmin) + " " + lremM1(lmin));
        System.out.println("fact div-neg-const " + idivNeg3(10) + " " + iremNeg3(-10) + " "
                + idivNeg3(-10) + " " + iremNeg3(10) + " " + ldivNeg3(10L) + " " + lremNeg3(-10L));
        System.out.println("fact div-pow2 " + idivPow2(-7) + " " + iremPow2(-7) + " " + idivPow2(-4)
                + " " + iremPow2(-1) + " " + ldivPow2(-7L) + " " + lremPow2(-7L) + " "
                + ldivPow2(-4L) + " " + lremPow2(-1L));
    }

    static void shiftAndConvRows() {
        for (int i = 0; i < WARM; i++) {
            sink += shl(i, i) + ushr(i, i) + lshl(i, i) + lushr(i, i) + sar(i, i);
            sink += i2b(i) + i2c(i) + i2s(i) + f2i(i) + d2l(i) + d2i(i);
            sink += (long) frem(i, 3f) + (long) drem(i, 3.0) + (long) fneg(i) + (long) dneg(i);
        }
        System.out.println("fact shift " + shl(1, 33) + " " + ushr(-1, 0) + " " + ushr(-1, 32) + " "
                + lshl(1L, 65) + " " + sar(-1, 31) + " " + ushr(-1, 34) + " " + lushr(-1L, 32) + " "
                + shl(1, -1));
        System.out.println("fact narrow " + i2b(200) + " " + i2c(-1) + " " + i2s(40000) + " "
                + i2b(-1) + " " + i2c(65536) + " " + i2b(128));
        System.out.println("fact f2i " + f2i(Float.NaN) + " " + f2i(1e20f) + " " + f2i(-1e20f) + " "
                + d2l(Double.NaN) + " " + d2l(Double.POSITIVE_INFINITY) + " "
                + d2l(Double.NEGATIVE_INFINITY) + " " + d2i(3e9) + " " + d2i(-3e9) + " "
                + d2i(3.99) + " " + d2i(-3.99));
        System.out.println("fact frem " + frem(-5.5f, 2f) + " " + drem(5.5, -2.0) + " "
                + frem(1f, 0f) + " " + drem(Double.POSITIVE_INFINITY, 2.0) + " "
                + drem(-0.0, 3.0) + " " + frem(0.0f, -3f));
        System.out.println("fact fneg " + fneg(0.0f) + " " + dneg(-0.0) + " "
                + Float.isNaN(fneg(Float.NaN)));
    }

    static void localsRows() {
        int[] ints = {1, 2, 3};
        J27BtBox box = new J27BtBox();
        for (int i = 0; i < WARM; i++) {
            sink += localsAfterAioobe(ints, i % 3) + localsAfterDiv(i | 1) + localsAfterNpe(box)
                    + localsAfterCce("abc");
        }
        System.out.println("fact locals-after-aioobe " + localsAfterAioobe(ints, 3));
        System.out.println("fact locals-after-div " + localsAfterDiv(0));
        System.out.println("fact locals-after-npe " + localsAfterNpe(null));
        System.out.println("fact locals-after-cce " + localsAfterCce(Integer.valueOf(1)));
    }

    static void typeRows() {
        Object str = "s";
        Object num = Integer.valueOf(5);
        row("newarray-neg", () -> sink += newInts(3).length, () -> sink += newInts(-5).length);
        row("anewarray-neg", () -> sink += newStrs(3).length, () -> sink += newStrs(-5).length);
        row("multianewarray-inner-neg", () -> sink += newMulti(2, 2).length, () -> sink += newMulti(2, -3).length);
        row(
                "multianewarray-outer-first",
                () -> sink += newMulti3(1, 1, 1).length,
                () -> sink += newMulti3(-1, 4, -2).length);
        row(
                "multianewarray-zero-then-neg",
                () -> sink += newMulti3a(1, 1, 1).length,
                () -> sink += newMulti3a(2, 0, -1).length);
        row(
                "multianewarray-zero-outer",
                () -> sink += newMulti3b(1, 1, 1).length,
                () -> sink += newMulti3b(0, -1, -1).length);
        row("checkcast-string", () -> sink += castStr(str).length(), () -> sink += castStr(num).length());
        row("checkcast-app", () -> sink += castBox(new J27BtBox()).v, () -> sink += castBox(num).v);
        int[] ints = {1};
        row("checkcast-prim-array", () -> sink += castInts(ints).length, () -> sink += castInts(new long[0]).length);
        String nulls = null;
        for (int i = 0; i < 3; i++) {
            nulls = castStr(null);
        }
        System.out.println("fact checkcast-null ok " + nulls);
        int isNull = 0;
        for (int i = 0; i < WARM; i++) {
            isNull += isStr(str) ? 0 : 1;
        }
        System.out.println("fact instanceof-null " + (isStr(null) ? 1 : 0) + (isNull == 0 ? "" : " warm-bad"));
    }

    // ---- safepoint polls in a call-free backward branch --------------------

    static volatile boolean stop;
    static volatile long spun;

    static long spin() {
        long n = 0;
        while (!stop) {
            n++;
        }
        return n;
    }

    static void pollRows() throws InterruptedException {
        Thread watchdog = new Thread(() -> {
            try {
                Thread.sleep(60_000);
            } catch (InterruptedException e) {
                return;
            }
            System.out.println("fact poll-watchdog timeout");
            System.exit(3);
        });
        watchdog.setDaemon(true);
        watchdog.start();
        Thread t = new Thread(() -> spun = spin(), "j27-spin");
        t.start();
        Thread.sleep(300);
        StackTraceElement[] st = t.getStackTrace();
        String top = st.length == 0 ? "<none>" : st[0].getMethodName();
        System.out.println("fact poll-gettrace ok top=" + top);
        System.gc();
        System.out.println("fact poll-gc ok");
        stop = true;
        t.join();
        System.out.println("fact poll-spin-stopped " + (spun > 0 ? "ok" : "zero"));
        watchdog.interrupt();
    }

    public static void main(String[] args) throws InterruptedException {
        arrayRows();
        objectRows();
        arithRows();
        typeRows();
        shiftAndConvRows();
        localsRows();
        pollRows();
        if (Boolean.getBoolean("j27.sink")) {
            System.out.println("sink " + sink);
        }
    }
}

class J27BtBox {
    int v = 1;

    int get() {
        return v;
    }
}
