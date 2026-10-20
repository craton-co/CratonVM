// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L7 (hunt): VM-built type-error texts in both modes --
// aastore's ArrayStoreException (primitive arrays, app arrays, arrays of arrays, an interface
// array), checkcast's ClassCastException with its module/loader clause (app class to JDK class,
// app array to JDK array, primitive arrays, arrays of arrays, app to app, app array to an
// interface), anewarray / newarray / multianewarray NegativeArraySizeException, integer and long
// division by zero, the Math.*Exact overflow texts and the top frame of their exception (HotSpot
// throws from the JDK body: `java.lang.Math.addExact`), and StackOverflowError (no message, a
// trace capped at MaxJavaStackTraceDepth = 1024 frames, the recursing method on top).
// Each row throws once (cold), then once more after 20000 non-throwing warm-up calls.
//
// Warm rows print the exception class only (HotSpot's -XX:+OmitStackTraceInFastThrow may drop
// the message and frames once C2 compiles a site); the cold rows carry the texts.
//
//   javac -d out L7W106VmTypeErrorTexts.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L7W106VmTypeErrorTexts
// HOTSPOT_EXPECTED_BEGIN
// cold ase-int-array -> java.lang.ArrayStoreException: [I
// cold ase-string -> java.lang.ArrayStoreException: java.lang.String
// cold ase-app-array -> java.lang.ArrayStoreException: [LL7W106VmTypeErrorTexts$Node;
// cold ase-array-of-arrays -> java.lang.ArrayStoreException: [[I
// cold ase-interface-array -> java.lang.ArrayStoreException: L7W106VmTypeErrorTexts$Node
// cold cce-app-to-jdk -> java.lang.ClassCastException: class L7W106VmTypeErrorTexts$Node cannot be cast to class java.lang.String (L7W106VmTypeErrorTexts$Node is in unnamed module of loader 'app'; java.lang.String is in module java.base of loader 'bootstrap')
// cold cce-app-array-to-jdk-array -> java.lang.ClassCastException: class [LL7W106VmTypeErrorTexts$Node; cannot be cast to class [Ljava.lang.String; ([LL7W106VmTypeErrorTexts$Node; is in unnamed module of loader 'app'; [Ljava.lang.String; is in module java.base of loader 'bootstrap')
// cold cce-primitive-arrays -> java.lang.ClassCastException: class [I cannot be cast to class [J ([I and [J are in module java.base of loader 'bootstrap')
// cold cce-2d-to-object-2d -> java.lang.ClassCastException: class [[I cannot be cast to class [[Ljava.lang.Object; ([[I and [[Ljava.lang.Object; are in module java.base of loader 'bootstrap')
// cold cce-app-to-app -> java.lang.ClassCastException: class L7W106VmTypeErrorTexts$Node cannot be cast to class L7W106VmTypeErrorTexts$Leaf (L7W106VmTypeErrorTexts$Node and L7W106VmTypeErrorTexts$Leaf are in unnamed module of loader 'app')
// cold cce-app-array-to-interface -> java.lang.ClassCastException: class [LL7W106VmTypeErrorTexts$Node; cannot be cast to class java.lang.Runnable ([LL7W106VmTypeErrorTexts$Node; is in unnamed module of loader 'app'; java.lang.Runnable is in module java.base of loader 'bootstrap')
// cold nase-anewarray -> java.lang.NegativeArraySizeException: -7
// cold nase-newarray -> java.lang.NegativeArraySizeException: -1
// cold nase-multi-outer -> java.lang.NegativeArraySizeException: -2
// cold nase-multi-inner -> java.lang.NegativeArraySizeException: -2
// cold nase-multi-after-zero -> java.lang.NegativeArraySizeException: -2
// cold idiv-zero -> java.lang.ArithmeticException: / by zero
// cold lrem-zero -> java.lang.ArithmeticException: / by zero
// cold add-exact-int -> java.lang.ArithmeticException: integer overflow @ java.lang.Math.addExact
// cold multiply-exact-long -> java.lang.ArithmeticException: long overflow @ java.lang.Math.multiplyExact
// cold to-int-exact -> java.lang.ArithmeticException: integer overflow @ java.lang.Math.toIntExact
// cold negate-exact-int -> java.lang.ArithmeticException: integer overflow @ java.lang.Math.negateExact
// cold abs-exact-long -> java.lang.ArithmeticException: Overflow to represent absolute value of Long.MIN_VALUE @ java.lang.Math.absExact
// cold divide-exact-int -> java.lang.ArithmeticException: integer overflow @ java.lang.Math.divideExact
// cold floor-mod-zero -> java.lang.ArithmeticException: / by zero @ java.lang.Math.floorMod
// cold soe -> java.lang.StackOverflowError: null frames=1024 top=L7W106VmTypeErrorTexts.recurse
// warm ase-int-array -> java.lang.ArrayStoreException
// warm ase-string -> java.lang.ArrayStoreException
// warm ase-app-array -> java.lang.ArrayStoreException
// warm ase-array-of-arrays -> java.lang.ArrayStoreException
// warm ase-interface-array -> java.lang.ArrayStoreException
// warm cce-app-to-jdk -> java.lang.ClassCastException
// warm cce-app-array-to-jdk-array -> java.lang.ClassCastException
// warm cce-primitive-arrays -> java.lang.ClassCastException
// warm cce-2d-to-object-2d -> java.lang.ClassCastException
// warm cce-app-to-app -> java.lang.ClassCastException
// warm cce-app-array-to-interface -> java.lang.ClassCastException
// warm nase-anewarray -> java.lang.NegativeArraySizeException
// warm nase-newarray -> java.lang.NegativeArraySizeException
// warm nase-multi-outer -> java.lang.NegativeArraySizeException
// warm nase-multi-inner -> java.lang.NegativeArraySizeException
// warm nase-multi-after-zero -> java.lang.NegativeArraySizeException
// warm idiv-zero -> java.lang.ArithmeticException
// warm lrem-zero -> java.lang.ArithmeticException
// warm add-exact-int -> java.lang.ArithmeticException
// warm multiply-exact-long -> java.lang.ArithmeticException
// warm to-int-exact -> java.lang.ArithmeticException
// warm negate-exact-int -> java.lang.ArithmeticException
// warm abs-exact-long -> java.lang.ArithmeticException
// warm divide-exact-int -> java.lang.ArithmeticException
// warm floor-mod-zero -> java.lang.ArithmeticException
// warm soe -> java.lang.StackOverflowError: null frames=1024 top=L7W106VmTypeErrorTexts.recurse
// HOTSPOT_EXPECTED_END
public class L7W106VmTypeErrorTexts {
    static final class Node {
    }

    static final class Leaf {
    }

    static int sink;
    static int depth;

    static void store(Object[] a, Object v) {
        a[0] = v;
    }

    static String castString(Object o) {
        return (String) o;
    }

    static String[] castStringArray(Object o) {
        return (String[]) o;
    }

    static long[] castLongArray(Object o) {
        return (long[]) o;
    }

    static Object[][] castObject2d(Object o) {
        return (Object[][]) o;
    }

    static Leaf castLeaf(Object o) {
        return (Leaf) o;
    }

    static Runnable castRunnable(Object o) {
        return (Runnable) o;
    }

    static Object anew(int n) {
        return new Node[n];
    }

    static Object newarr(int n) {
        return new int[n];
    }

    static Object multi(int a, int b) {
        return new long[a][b];
    }

    static int idiv(int a, int b) {
        return a / b;
    }

    static long lrem(long a, long b) {
        return a % b;
    }

    static void recurse() {
        depth++;
        recurse();
    }

    // Warm rows print the exception CLASS only: once C2 has compiled a site, HotSpot may throw a
    // preallocated stackless exception without a message (-XX:+OmitStackTraceInFastThrow), so the
    // text and frames there depend on JIT timing. The texts are checked by the cold rows.
    static final String WARM = "warm";

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String phase, String name, boolean frame, Row r) {
        String answer;
        try {
            answer = "no exception: " + r.run();
        } catch (StackOverflowError e) {
            StackTraceElement[] st = e.getStackTrace();
            answer = e.getClass().getName() + ": " + e.getMessage() + " frames=" + st.length + " top="
                + (st.length == 0 ? "-" : st[0].getClassName() + "." + st[0].getMethodName());
        } catch (Throwable t) {
            answer = t.toString();
            if (WARM.equals(phase)) {
                answer = t.getClass().getName();
            } else if (frame) {
                StackTraceElement[] st = t.getStackTrace();
                answer += " @ " + (st.length == 0 ? "-" : st[0].getClassName() + "." + st[0].getMethodName());
            }
        }
        System.out.println(phase + " " + name + " -> " + answer);
    }

    static int MAXI = Integer.MAX_VALUE;
    static int MINI = Integer.MIN_VALUE;
    static long MAXL = Long.MAX_VALUE;
    static long MINL = Long.MIN_VALUE;

    static void rows(String phase) {
        row(phase, "ase-int-array", false, () -> {
            store(new Integer[1], new int[0]);
            return "stored";
        });
        row(phase, "ase-string", false, () -> {
            store(new Integer[1], "s");
            return "stored";
        });
        row(phase, "ase-app-array", false, () -> {
            store(new Integer[1], new Node[0]);
            return "stored";
        });
        row(phase, "ase-array-of-arrays", false, () -> {
            store(new Node[1][], new int[0][]);
            return "stored";
        });
        row(phase, "ase-interface-array", false, () -> {
            store(new Runnable[1], new Node());
            return "stored";
        });
        row(phase, "cce-app-to-jdk", false, () -> castString(new Node()));
        row(phase, "cce-app-array-to-jdk-array", false, () -> castStringArray(new Node[0]));
        row(phase, "cce-primitive-arrays", false, () -> castLongArray(new int[0]));
        row(phase, "cce-2d-to-object-2d", false, () -> castObject2d(new int[1][1]));
        row(phase, "cce-app-to-app", false, () -> castLeaf(new Node()));
        row(phase, "cce-app-array-to-interface", false, () -> castRunnable(new Node[0]));
        row(phase, "nase-anewarray", false, () -> anew(-7));
        row(phase, "nase-newarray", false, () -> newarr(-1));
        row(phase, "nase-multi-outer", false, () -> multi(-2, 3));
        row(phase, "nase-multi-inner", false, () -> multi(2, -2));
        row(phase, "nase-multi-after-zero", false, () -> multi(0, -2));
        row(phase, "idiv-zero", false, () -> idiv(7, 0));
        row(phase, "lrem-zero", false, () -> lrem(7L, 0L));
        row(phase, "add-exact-int", true, () -> Math.addExact(MAXI, 1));
        row(phase, "multiply-exact-long", true, () -> Math.multiplyExact(MAXL, 2L));
        row(phase, "to-int-exact", true, () -> Math.toIntExact(MAXL));
        row(phase, "negate-exact-int", true, () -> Math.negateExact(MINI));
        row(phase, "abs-exact-long", true, () -> Math.absExact(MINL));
        row(phase, "divide-exact-int", true, () -> Math.divideExact(MINI, -1));
        row(phase, "floor-mod-zero", true, () -> Math.floorMod(5L, 0L));
        depth = 0;
        row(phase, "soe", false, () -> {
            recurse();
            return "returned";
        });
    }

    static void warm() {
        Object[] objs = new Object[1];
        Node node = new Node();
        for (int i = 0; i < 20_000; i++) {
            store(objs, node);
            sink += castString("x").length() + castStringArray(new String[0]).length
                + castLongArray(new long[0]).length + castObject2d(new Object[0][]).length;
            sink += castLeaf(new Leaf()) == null ? 1 : 0;
            sink += castRunnable(null) == null ? 1 : 0;
            sink += ((Object[]) anew(1)).length + ((int[]) newarr(1)).length + ((long[][]) multi(1, 1)).length;
            sink += idiv(i, 3) + (int) lrem(i, 7L);
            sink += Math.addExact(i, 1) + (int) Math.multiplyExact((long) i, 2L) + Math.toIntExact((long) i)
                + Math.negateExact(i) + (int) Math.absExact((long) -i) + Math.divideExact(i, 3)
                + (int) Math.floorMod((long) i, 5L);
        }
    }

    public static void main(String[] args) {
        rows("cold");
        warm();
        rows("warm");
    }
}
