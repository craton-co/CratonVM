// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Constructor;
import java.lang.reflect.Method;

/**
 * SBR round 5, lane {@code sysprops}: where the cost of {@code Constructor.newInstance} sits.
 * ns/op, best of 5 after a warm-up. Usage: {@code Sbr5SyspropsCtor <row> [iterations]}.
 */
public final class Sbr5SyspropsCtor {
    public static final class Pt {
        public int x;
        public Pt() {}
        public Pt(int x) { this.x = x; }
        public static int noop(int a) { return a + 1; }
        public int inst(int a) { return a + x; }
    }

    interface Op { void run(int n) throws Exception; }

    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String row = args[0];
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        Constructor<Pt> c0 = Pt.class.getConstructor();
        Constructor<Pt> c1 = Pt.class.getConstructor(int.class);
        Method st = Pt.class.getMethod("noop", int.class);
        Method in = Pt.class.getMethod("inst", int.class);
        Pt pt = new Pt(3);
        Op op;
        switch (row) {
            case "new": op = k -> { for (int i = 0; i < k; i++) sink = new Pt(); }; break;
            case "ctor0": op = k -> { for (int i = 0; i < k; i++) sink = c0.newInstance(); }; break;
            case "ctor1": op = k -> { for (int i = 0; i < k; i++) sink = c1.newInstance(i); }; break;
            case "invokeStatic": op = k -> { for (int i = 0; i < k; i++) sink = st.invoke(null, i); }; break;
            case "invokeInst": op = k -> { for (int i = 0; i < k; i++) sink = in.invoke(pt, i); }; break;
            case "classNewInstance": op = k -> { for (int i = 0; i < k; i++) sink = Pt.class.getDeclaredConstructor().newInstance(); }; break;
            case "nanoTime": op = k -> { long t = 0; for (int i = 0; i < k; i++) t += System.nanoTime(); sink = t; }; break;
            case "identityHash": { Object o = new Object(); op = k -> { long t = 0; for (int i = 0; i < k; i++) t += System.identityHashCode(o); sink = t; }; break; }
            case "newArrayReflect": op = k -> { for (int i = 0; i < k; i++) sink = java.lang.reflect.Array.newInstance(int.class, 4); }; break;
            case "getCtor": op = k -> { for (int i = 0; i < k; i++) sink = Pt.class.getConstructor(); }; break;
            default: throw new IllegalArgumentException(row);
        }
        op.run(n);
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < 5; rep++) {
            long t0 = System.nanoTime();
            op.run(n);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println(row + " " + (best / n) + " ns/op");
    }
}
