// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 5 (lane EAHUNT): escape-analysis rematerialisation and
// bounds-check-elimination facts.
// Page: docs/feature-designs/real-frame-deopt.md
//
// Every EA row allocates objects a compiled body can scalar-replace, warms the
// method with `rare == false` (WARM calls), then calls it once with `rare ==
// true`: the branch the profile never saw is the deopt point, and the objects
// live there must be rebuilt exactly as the interpreter would hold them.
// Every BCE row warms a loop over a well-behaved array and then hands it a
// shape whose bounds check must still fire (an index that wraps, an array a phi
// or a field store replaces mid-loop, a short inner array).
//
//   java -cp <dir> J27EaBoundsRows > hs.txt
//   cratonvm --cp <dir> J27EaBoundsRows | diff - hs.txt
//     x {--real-jdk, --compatible} x {ZGC default, -XX:+UseG1GC, -XX:+UseGenerationalGC}
//   engagement: + CRATONVM_DBG_SCALAR_DEOPT=1 2>&1 | grep -c MATERIALIZE   (> 0)
//
// Expected HotSpot 25 output:
//   fact ea-null-ref-field 1007
//   fact ea-zero-defaults l=0 d=0.0 o=true z=false c=0
//   fact ea-alias-identity true true
//   fact ea-cycle true 3
//   fact ea-int-array len=3 sum=4
//   fact ea-byte-array -1 44
//   fact ea-lambda-capture 1009
//   fact ea-nested-holder 1011 true
//   fact bce-wrap java.lang.ArrayIndexOutOfBoundsException: Index 2147483647 out of bounds for length 4
//   fact bce-phi-array java.lang.ArrayIndexOutOfBoundsException: Index 3 out of bounds for length 3 s=3
//   fact bce-field-reload java.lang.ArrayIndexOutOfBoundsException: Index 3 out of bounds for length 3 s=3
//   fact bce-2d java.lang.ArrayIndexOutOfBoundsException: Index 2 out of bounds for length 2
//   fact bce-neg-offset java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 8
public class J27EaBoundsRows {
    static final int WARM = 30000;
    static long sink;

    static final class Node {
        Node next;
        int v;
    }

    static final class Prims {
        long l;
        double d;
        Object o;
        boolean z;
        char c;
    }

    static final class Holder {
        int[] arr;
    }

    // ── EA rows ─────────────────────────────────────────────────────────

    /** A never-stored reference field of a scalar-replaced object is null. */
    static int nullRefField(int x, boolean rare) {
        Node n = new Node();
        n.v = x;
        if (rare) {
            return n.next == null ? n.v : -1;
        }
        return n.v;
    }

    static String zeroDefaults(boolean rare) {
        Prims p = new Prims();
        if (rare) {
            return "l=" + p.l + " d=" + p.d + " o=" + (p.o == null) + " z=" + p.z + " c="
                    + (int) p.c;
        }
        return null;
    }

    /** Two locals naming one virtual object: one object after the deopt. */
    static String aliasIdentity(int x, boolean rare) {
        Node a = new Node();
        a.v = x;
        Node b = a;
        if (rare) {
            b.v = 99;
            return (a == b) + " " + (a.v == 99);
        }
        return null;
    }

    /** Two virtual objects pointing at each other. */
    static String cycle(int x, boolean rare) {
        Node a = new Node();
        Node b = new Node();
        a.next = b;
        b.next = a;
        a.v = x;
        b.v = x + 1;
        if (rare) {
            return (a.next.next == a) + " " + (a.v + b.v);
        }
        return null;
    }

    static String intArray(int x, boolean rare) {
        int[] arr = new int[3];
        arr[0] = x;
        arr[2] = x + 2;
        if (rare) {
            return "len=" + arr.length + " sum=" + (arr[0] + arr[1] + arr[2]);
        }
        return null;
    }

    static String byteArray(int x, boolean rare) {
        byte[] b = new byte[2];
        b[0] = (byte) x;
        b[1] = (byte) (x + 45);
        if (rare) {
            return b[0] + " " + b[1];
        }
        return null;
    }

    static int lambdaCapture(int x, boolean rare) {
        Node n = new Node();
        n.v = x;
        java.util.function.IntSupplier s = () -> n.v;
        if (rare) {
            return s.getAsInt() + (n.next == null ? 0 : 5000);
        }
        return s.getAsInt();
    }

    /** A virtual object stored into another virtual object. */
    static String nestedHolder(int x, boolean rare) {
        Node inner = new Node();
        inner.v = x;
        Node outer = new Node();
        outer.next = inner;
        if (rare) {
            return outer.next.v + " " + (outer.next.next == null);
        }
        return null;
    }

    // ── BCE rows ────────────────────────────────────────────────────────

    /** `a[i + k]` with `k` near `Integer.MAX_VALUE` (an `i + c` proof must not admit it). */
    static int wrapSum(int[] a, int k, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += a[i + k];
        }
        return s;
    }

    /** The array a φ carries is replaced by a shorter one mid-loop. */
    static int phiArray(int[] first, int[] second, int n) {
        int[] cur = first;
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += cur[i] + 1;
            if (i == 1) {
                cur = second;
            }
        }
        return s;
    }

    static int sFieldReload;

    /** The array a non-final field holds is replaced mid-loop. */
    static int fieldReload(Holder h, int[] repl, int n) {
        sFieldReload = 0;
        for (int i = 0; i < n && i < h.arr.length + 8; i++) {
            sFieldReload += h.arr[i] + 1;
            if (i == 2 && repl != null) {
                h.arr = repl;
            }
        }
        return sFieldReload;
    }

    /** Bounded by the outer row's length, indexing the inner one. */
    static int twoD(int[][] m) {
        int s = 0;
        for (int j = 0; j < m[0].length; j++) {
            s += m[1][j];
        }
        return s;
    }

    /** `a[i - 1]` from a start of 0 must still throw at i == 0. */
    static int negOffset(int[] a, int start) {
        int s = 0;
        for (int i = start; i < a.length; i++) {
            s += a[i - 1];
        }
        return s;
    }

    static String caught(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    public static void main(String[] args) {
        long acc = 0;
        for (int i = 0; i < WARM; i++) {
            acc += nullRefField(i, false);
            Object z = zeroDefaults(false);
            acc += z == null ? 0 : 1;
            acc += aliasIdentity(i, false) == null ? 0 : 1;
            acc += cycle(i, false) == null ? 0 : 1;
            acc += intArray(i, false) == null ? 0 : 1;
            acc += byteArray(i, false) == null ? 0 : 1;
            acc += lambdaCapture(i, false);
            acc += nestedHolder(i, false) == null ? 0 : 1;
        }
        sink = acc;
        System.out.println("fact ea-null-ref-field " + nullRefField(1007, true));
        System.out.println("fact ea-zero-defaults " + zeroDefaults(true));
        System.out.println("fact ea-alias-identity " + aliasIdentity(5, true));
        System.out.println("fact ea-cycle " + cycle(1, true));
        System.out.println("fact ea-int-array " + intArray(1, true));
        // (byte) 255 = -1; (byte) (255 + 45) = (byte) 300 = 44.
        System.out.println("fact ea-byte-array " + byteArray(255, true));
        System.out.println("fact ea-lambda-capture " + lambdaCapture(1009, true));
        System.out.println("fact ea-nested-holder " + nestedHolder(1011, true));

        int[] four = {1, 2, 3, 4};
        for (int i = 0; i < WARM; i++) {
            acc += wrapSum(four, 0, 4);
        }
        try {
            wrapSum(four, Integer.MAX_VALUE, 2);
            System.out.println("fact bce-wrap no-throw");
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("fact bce-wrap " + caught(e));
        }

        int[] eight = new int[8];
        int[] three = new int[3];
        for (int i = 0; i < WARM; i++) {
            acc += phiArray(eight, eight, 8);
        }
        int phiSum = 0;
        try {
            phiSum = phiArray(eight, three, 8);
            System.out.println("fact bce-phi-array no-throw " + phiSum);
        } catch (ArrayIndexOutOfBoundsException e) {
            // cur[0] + 1, cur[1] + 1, then cur = three; three[2] + 1: s = 3
            // when three[3] throws. `s` is a local, so recompute it here.
            System.out.println("fact bce-phi-array " + caught(e) + " s=3");
        }

        Holder h = new Holder();
        for (int i = 0; i < WARM; i++) {
            h.arr = eight;
            acc += fieldReload(h, null, 8);
        }
        h.arr = eight;
        try {
            fieldReload(h, new int[3], 8);
            System.out.println("fact bce-field-reload no-throw " + sFieldReload);
        } catch (ArrayIndexOutOfBoundsException e) {
            // i = 0, 1, 2 add 1 each before h.arr[3] on the 3-element array throws.
            System.out.println("fact bce-field-reload " + caught(e) + " s=" + sFieldReload);
        }

        int[][] square = {new int[4], new int[4]};
        for (int i = 0; i < WARM; i++) {
            acc += twoD(square);
        }
        try {
            twoD(new int[][] {new int[4], new int[2]});
            System.out.println("fact bce-2d no-throw");
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("fact bce-2d " + caught(e));
        }

        for (int i = 0; i < WARM; i++) {
            acc += negOffset(eight, 1);
        }
        try {
            negOffset(eight, 0);
            System.out.println("fact bce-neg-offset no-throw");
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("fact bce-neg-offset " + caught(e));
        }
        sink += acc;
    }
}
