// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// JIT round 18 wave 9 (lane osrpoly): one call of `poly` runs a hot loop over
// a three-class interface site (50/25/25 %), so the loop lives in an OSR body.
// The single-pass OSR body splices the site polymorphically; the optimizing
// OSR door must not replace it with an opaque body that keeps the dispatch.
public final class OsrPolyProbe {
    interface Shape { int area(); }
    static final class Sq implements Shape { final int s; Sq(int s) { this.s = s; } public int area() { return s * s; } }
    static final class Re implements Shape { final int a, b; Re(int a, int b) { this.a = a; this.b = b; } public int area() { return a * b; } }
    static final class Tr implements Shape { final int b, h; Tr(int b, int h) { this.b = b; this.h = h; } public int area() { return b * h / 2; } }

    static long poly(int n) {
        Shape[] sh = { new Sq(3), new Re(2, 5), new Tr(4, 6), new Sq(7) };
        long s = 0;
        for (int i = 0; i < n; i++) s += sh[i & 3].area();
        return s;
    }

    public static void main(String[] args) {
        System.out.println("poly " + poly(3_000_000));
        System.out.println("poly " + poly(1_000_003));
        System.out.println("OSR-POLY-OK");
    }
}
