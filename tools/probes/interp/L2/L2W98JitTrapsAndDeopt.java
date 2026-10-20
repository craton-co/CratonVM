// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe -- Interpreter round i4 wave 98, lane L2 (compiled code vs the
// interpreter, both against HotSpot 25). Each method is warmed past the JIT
// thresholds (WARM calls on the good path) and only THEN driven into its
// exception / deoptimization: array bounds, negative length, array store and
// cast checks, a division by zero, an implicit null check, exceptions thrown
// from a callee that may be inlined (the trace must keep both frames with
// their own line numbers), and deoptimizations with every kind of local live
// (a new receiver class arriving after a monomorphic warm-up, an untaken
// branch taken, an OSR'd loop leaving on its last iteration); deopt rows print
// every live local. Exception rows: class, message, first two frames.
//
// Run (compile WITHOUT -g: the NPE texts name `<parameterN>`/`<localN>`):
//   $J/javac -d out tools/probes/interp/L2/L2W98JitTrapsAndDeopt.java
//   $J/java -cp out L2W98JitTrapsAndDeopt > hs.txt
//   $B --java-home $J/.. -cp out L2W98JitTrapsAndDeopt | diff hs.txt -
//   $B --java-home $J/.. --nojit -cp out L2W98JitTrapsAndDeopt | diff hs.txt -  (and --compatible)
//
// HOTSPOT_EXPECTED_BEGIN
// warm ok
// aload(5) java.lang.ArrayIndexOutOfBoundsException: Index 5 out of bounds for length 5
//   L2W98JitTrapsAndDeopt.aload(L2W98JitTrapsAndDeopt.java:103)
//   L2W98JitTrapsAndDeopt.lambda$main$0(L2W98JitTrapsAndDeopt.java:277)
// aload(-1) java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 5
//   L2W98JitTrapsAndDeopt.aload(L2W98JitTrapsAndDeopt.java:103)
//   L2W98JitTrapsAndDeopt.lambda$main$1(L2W98JitTrapsAndDeopt.java:278)
// lstore(MIN) java.lang.ArrayIndexOutOfBoundsException: Index -2147483648 out of bounds for length 5
//   L2W98JitTrapsAndDeopt.lstore(L2W98JitTrapsAndDeopt.java:108)
//   L2W98JitTrapsAndDeopt.lambda$main$2(L2W98JitTrapsAndDeopt.java:279)
// newArr(-1) java.lang.NegativeArraySizeException: -1
//   L2W98JitTrapsAndDeopt.newArr(L2W98JitTrapsAndDeopt.java:112)
//   L2W98JitTrapsAndDeopt.lambda$main$3(L2W98JitTrapsAndDeopt.java:280)
// newObjArr(-5) java.lang.NegativeArraySizeException: -5
//   L2W98JitTrapsAndDeopt.newObjArr(L2W98JitTrapsAndDeopt.java:116)
//   L2W98JitTrapsAndDeopt.lambda$main$4(L2W98JitTrapsAndDeopt.java:281)
// newMulti(-3) java.lang.NegativeArraySizeException: -3
//   L2W98JitTrapsAndDeopt.newMulti(L2W98JitTrapsAndDeopt.java:120)
//   L2W98JitTrapsAndDeopt.lambda$main$5(L2W98JitTrapsAndDeopt.java:282)
// astore(Integer) java.lang.ArrayStoreException
// astore(int[]) java.lang.ArrayStoreException
// cast(Integer) java.lang.ClassCastException: class java.lang.Integer cannot be cast to class java.lang.String (java.lang.Integer and java.lang.String are in module java.base of loader 'bootstrap')
//   L2W98JitTrapsAndDeopt.cast(L2W98JitTrapsAndDeopt.java:128)
//   L2W98JitTrapsAndDeopt.lambda$main$8(L2W98JitTrapsAndDeopt.java:285)
// castArr(Object[]) java.lang.ClassCastException: class [Ljava.lang.Object; cannot be cast to class [Ljava.lang.String; ([Ljava.lang.Object; and [Ljava.lang.String; are in module java.base of loader 'bootstrap')
//   L2W98JitTrapsAndDeopt.castArr(L2W98JitTrapsAndDeopt.java:133)
//   L2W98JitTrapsAndDeopt.lambda$main$9(L2W98JitTrapsAndDeopt.java:286)
// div(0) java.lang.ArithmeticException: / by zero
//   L2W98JitTrapsAndDeopt.div(L2W98JitTrapsAndDeopt.java:137)
//   L2W98JitTrapsAndDeopt.lambda$main$10(L2W98JitTrapsAndDeopt.java:287)
// len(null) java.lang.NullPointerException: Cannot read the array length because "<parameter1>" is null
//   L2W98JitTrapsAndDeopt.len(L2W98JitTrapsAndDeopt.java:141)
//   L2W98JitTrapsAndDeopt.lambda$main$11(L2W98JitTrapsAndDeopt.java:288)
// outer(-1) java.lang.IllegalStateException: inner -1
//   L2W98JitTrapsAndDeopt.inner(L2W98JitTrapsAndDeopt.java:146)
//   L2W98JitTrapsAndDeopt.outer(L2W98JitTrapsAndDeopt.java:153)
// arraycopy(int->long) java.lang.ArrayStoreException: arraycopy: type mismatch: can not copy int[] into long[]
//   java.base/java.lang.System.arraycopy(Native Method)
//   L2W98JitTrapsAndDeopt.copy(L2W98JitTrapsAndDeopt.java:157)
// arraycopy(range) java.lang.ArrayIndexOutOfBoundsException: arraycopy: last source index 12 out of bounds for int[10]
//   java.base/java.lang.System.arraycopy(Native Method)
//   L2W98JitTrapsAndDeopt.copy(L2W98JitTrapsAndDeopt.java:157)
// astoreCold(Integer) java.lang.ArrayStoreException: java.lang.Integer
//   L2W98JitTrapsAndDeopt.astoreCold(L2W98JitTrapsAndDeopt.java:313)
//   L2W98JitTrapsAndDeopt.lambda$main$15(L2W98JitTrapsAndDeopt.java:292)
// astoreCold(int[]) java.lang.ArrayStoreException: [I
//   L2W98JitTrapsAndDeopt.astoreCold(L2W98JitTrapsAndDeopt.java:313)
//   L2W98JitTrapsAndDeopt.lambda$main$16(L2W98JitTrapsAndDeopt.java:293)
// deopt-class 314|7000021|3.5|1.75|s3|8|7|h|false|9
// deopt-branch -7|-7000049|-3.5|-1.75|s1|-6|-7|t|false|neg
// osr-exit 1999|1999000000000|999.5|499.75|s3|2000|-49|x|false|last
// catch-in-loop 30000|38570|12345678901|2.5
// HOTSPOT_EXPECTED_END
public class L2W98JitTrapsAndDeopt {
    static final int WARM = 30000;

    abstract static class Shape {
        abstract int area();
    }

    static final class Square extends Shape {
        final int s;

        Square(int s) {
            this.s = s;
        }

        int area() {
            return s * s;
        }
    }

    // Loaded only when the deopt row runs: a new receiver class at a site the
    // warm-up saw monomorphic.
    static final class Rect extends Shape {
        int area() {
            return 314;
        }
    }

    static int aload(int[] a, int i) {
        int x = a[i];
        return x;
    }

    static void lstore(long[] a, int i) {
        a[i] = 42L;
    }

    static int newArr(int n) {
        return new int[n].length;
    }

    static int newObjArr(int n) {
        return new Object[n].length;
    }

    static int newMulti(int n) {
        return new int[2][n].length;
    }

    static void astore(Object[] a, Object v) {
        a[0] = v;
    }

    static int cast(Object o) {
        String s = (String) o;
        return s.length();
    }

    static int castArr(Object o) {
        return ((String[]) o).length;
    }

    static int div(int a, int b) {
        return a / b;
    }

    static int len(int[] a) {
        return a.length;
    }

    static int inner(int x) {
        if (x < 0) {
            throw new IllegalStateException("inner " + x);
        }
        return x + 1;
    }

    static int outer(int x) {
        int y = x * 2;
        return inner(x) + y;
    }

    static void copy(Object src, int sp, Object dst, int dp, int n) {
        System.arraycopy(src, sp, dst, dp, n);
    }

    static String deoptClass(Shape s, int i) {
        long l = i * 1000003L;
        double d = i * 0.5;
        float f = i * 0.25f;
        String str = "s" + (i & 3);
        int[] arr = {i, i + 1};
        byte b = (byte) i;
        char c = (char) ('a' + (i % 26));
        boolean z = (i & 1) == 0;
        int area = s.area();
        return area + "|" + l + "|" + d + "|" + f + "|" + str + "|" + arr[1] + "|" + b + "|" + c
            + "|" + z + "|" + (arr[0] + 2);
    }

    static String deoptBranch(int i) {
        long l = i * 1000007L;
        double d = i * 0.5;
        float f = i * 0.25f;
        String str = "s" + (i & 3);
        int[] arr = {i, i + 1};
        byte b = (byte) i;
        char c = (char) ('a' + Math.floorMod(i, 26));
        boolean z = (i & 1) == 0;
        String tag;
        if (i < 0) {
            tag = "neg";
        } else {
            tag = "pos";
        }
        return i + "|" + l + "|" + d + "|" + f + "|" + str + "|" + arr[1] + "|" + b + "|" + c
            + "|" + z + "|" + tag;
    }

    // One call, a long loop (only OSR can compile it); the last iteration
    // takes a branch the loop never took before, with every local live.
    static String osrExit(int n) {
        long l = 0;
        double d = 0;
        float f = 0;
        String str = "";
        int[] arr = new int[1];
        byte b = 0;
        char c = 'a';
        boolean z = true;
        String tag = "";
        int i;
        for (i = 0; i < n; i++) {
            l = i * 1000000000L;
            d = i * 0.5;
            f = i * 0.25f;
            arr[0] = i + 1;
            b = (byte) i;
            z = (i & 1) == 0;
            if (i == n - 1) {
                str = "s" + (i & 3);
                c = (char) ('a' + (i % 26));
                tag = "last";
                break;
            }
        }
        return i + "|" + l + "|" + d + "|" + f + "|" + str + "|" + arr[0] + "|" + b + "|" + c
            + "|" + z + "|" + tag;
    }

    // An exception thrown and caught inside an OSR'd loop, locals live across
    // the handler.
    static String catchInLoop(int n, int[] data) {
        int caught = 0;
        long big = 12345678901L;
        double half = 2.5;
        int i;
        for (i = 0; i < n; i++) {
            try {
                if (data[i % 7] == 0) {
                    caught++;
                }
            } catch (ArrayIndexOutOfBoundsException e) {
                caught += 2;
            }
        }
        return i + "|" + caught + "|" + big + "|" + half;
    }

    interface Thunk {
        Object get();
    }

    static void trap(String label, Thunk t) {
        try {
            System.out.println(label + " no-throw " + t.get());
        } catch (RuntimeException e) {
            System.out.println(label + " " + e);
            StackTraceElement[] st = e.getStackTrace();
            for (int k = 0; k < 2 && k < st.length; k++) {
                System.out.println("  " + st[k]);
            }
        }
    }

    public static void main(String[] args) {
        int[] five = {1, 2, 3, 4, 5};
        long[] lfive = new long[5];
        Object[] strs = new String[1];
        Square sq = new Square(3);
        long sink = 0;
        boolean ok = true;
        for (int i = 0; i < WARM; i++) {
            sink += aload(five, i % 5);
            lstore(lfive, i % 5);
            sink += newArr(i & 7) + newObjArr(i & 3) + newMulti(i & 1);
            astore(strs, "x");
            sink += cast("ab") + castArr(new String[2]) + div(i, 3) + len(five) + outer(i & 15);
            copy(five, 0, new int[5], 0, 5);
            ok &= deoptClass(sq, i).startsWith("9|");
            ok &= deoptBranch(i).endsWith("|pos");
        }
        System.out.println(ok && sink != 0 ? "warm ok" : "warm FAIL");
        trap("aload(5)", () -> aload(five, 5));
        trap("aload(-1)", () -> aload(five, -1));
        trap("lstore(MIN)", () -> { lstore(lfive, Integer.MIN_VALUE); return 0; });
        trap("newArr(-1)", () -> newArr(-1));
        trap("newObjArr(-5)", () -> newObjArr(-5));
        trap("newMulti(-3)", () -> newMulti(-3));
        trapQuiet("astore(Integer)", () -> { astore(strs, 7); return 0; });
        trapQuiet("astore(int[])", () -> { astore(strs, new int[0]); return 0; });
        trap("cast(Integer)", () -> cast(7));
        trap("castArr(Object[])", () -> castArr(new Object[1]));
        trap("div(0)", () -> div(1, 0));
        trap("len(null)", () -> len(null));
        trap("outer(-1)", () -> outer(-1));
        trap("arraycopy(int->long)", () -> { copy(five, 0, new long[5], 0, 5); return 0; });
        trap("arraycopy(range)", () -> { copy(new int[10], 2, new int[10], 0, 10); return 0; });
        trap("astoreCold(Integer)", () -> { astoreCold(strs, 7); return 0; });
        trap("astoreCold(int[])", () -> { astoreCold(strs, new int[0]); return 0; });
        System.out.println("deopt-class " + deoptClass(new Rect(), 7));
        System.out.println("deopt-branch " + deoptBranch(-7));
        System.out.println("osr-exit " + osrExit(2000));
        System.out.println("catch-in-loop " + catchInLoop(30000, new int[5]));
    }

    // HotSpot C2 may throw a hot implicit ArrayStoreException as a preallocated
    // instance with no message and no frames (-XX:+OmitStackTraceInFastThrow),
    // depending on compile timing; the warmed `astore` rows print the class
    // only, and this never-warmed twin checks the message and frames.
    static void trapQuiet(String label, Thunk t) {
        try {
            System.out.println(label + " no-throw " + t.get());
        } catch (RuntimeException e) {
            System.out.println(label + " " + e.getClass().getName());
        }
    }

    static void astoreCold(Object[] a, Object v) {
        a[0] = v;
    }
}
