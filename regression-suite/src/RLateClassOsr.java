// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Loops over classes that are first used AFTER the method's first loop was
// OSR-compiled. With the late-bind OSR exit the running body leaves at a
// back-edge poll for the rebuilt one; the result must be identical either way
// and the loop state (locals, a live monitor, a captured array) must survive
// the exit. Deterministic output; the timing line is a generous bound
// (the defect was ~320 ns/op against ~4 ns).

public class RLateClassOsr {
    static Object sink;

    static class P {
        final int a, b;
        P(int a, int b) { this.a = a; this.b = b; }
        int sum() { return a * 31 + b; }
    }

    static class Q extends P {
        final long c;
        Q(int a) { super(a, a ^ 5); this.c = a * 7L; }
    }

    static final Object LOCK = new Object();

    public static void main(String[] args) {
        long acc = 0;
        int[] arr = new int[64];
        // First loop: compiled by OSR before P and Q exist.
        for (int i = 0; i < 4_000_000; i++) {
            sink = new Object();
            arr[i & 63] += i;
        }
        for (int v : arr) acc += v;
        // Second loop, same method, classes first used here; hold a lock across it.
        long t0 = System.nanoTime();
        synchronized (LOCK) {
            for (int i = 0; i < 6_000_000; i++) {
                P p = new P(i, 2);
                Q q = new Q(i & 1023);
                acc += p.sum() + q.sum() + q.c;
                arr[(i * 5) & 63] ^= i;
                sink = p;
            }
        }
        long dt = System.nanoTime() - t0;
        for (int v : arr) acc = acc * 31 + v;
        System.out.println("CK acc=" + acc);
        System.out.println(dt / 6_000_000 < 200 ? "PASS RLateClassOsr (2 checks)" : "SLOW " + dt / 6_000_000 + " ns/iter");
    }
}
