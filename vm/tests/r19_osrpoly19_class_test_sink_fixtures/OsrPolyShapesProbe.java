// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// JIT round 19 wave 1 (lane osrpoly19): hot loops over three- and four-class
// interface sites and a three-class virtual site, each run in an OSR body,
// then one null element at the same site. Under the optimizing tier's
// polymorphic guarded splice the class tests form a chain on one receiver;
// sinking each test to its branch (CRATONVM_JIT_IR_SINK_CLASS_TEST_TO_BRANCH)
// must leave every sum, and the null receiver's NullPointerException, as
// HotSpot has them.
public final class OsrPolyShapesProbe {
    interface Shape { int area(); }
    static final class Sq implements Shape { final int s; Sq(int s) { this.s = s; } public int area() { return s * s; } }
    static final class Re implements Shape { final int a, b; Re(int a, int b) { this.a = a; this.b = b; } public int area() { return a * b; } }
    static final class Tr implements Shape { final int b, h; Tr(int b, int h) { this.b = b; this.h = h; } public int area() { return b * h / 2; } }
    static final class Ci implements Shape { final int r; Ci(int r) { this.r = r; } public int area() { return 3 * r * r; } }

    static abstract class Fig { abstract int area(); }
    static final class FSq extends Fig { final int s; FSq(int s) { this.s = s; } int area() { return s * s; } }
    static final class FRe extends Fig { final int a, b; FRe(int a, int b) { this.a = a; this.b = b; } int area() { return a * b; } }
    static final class FTr extends Fig { final int b, h; FTr(int b, int h) { this.b = b; this.h = h; } int area() { return b * h / 2; } }

    static long poly3(Shape[] sh, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += sh[i & 3].area();
        return s;
    }

    static long poly4(int n) {
        Shape[] sh = { new Sq(3), new Re(2, 5), new Tr(4, 6), new Ci(2) };
        long s = 0;
        for (int i = 0; i < n; i++) s += sh[i & 3].area();
        return s;
    }

    static long poly3v(int n) {
        Fig[] sh = { new FSq(3), new FRe(2, 5), new FTr(4, 6), new FSq(7) };
        long s = 0;
        for (int i = 0; i < n; i++) s += sh[i & 3].area();
        return s;
    }

    public static void main(String[] args) {
        Shape[] sh = { new Sq(3), new Re(2, 5), new Tr(4, 6), new Sq(7) };
        System.out.println("poly3 " + poly3(sh, 3_000_000));
        System.out.println("poly3 " + poly3(sh, 1_000_003));
        System.out.println("poly4 " + poly4(3_000_001));
        System.out.println("poly3v " + poly3v(3_000_002));
        sh[1] = null;
        try {
            System.out.println("null " + poly3(sh, 1_000));
        } catch (NullPointerException e) {
            System.out.println("npe " + e.getClass().getName());
        }
        System.out.println("OSR-POLY-SHAPES-OK");
    }
}
