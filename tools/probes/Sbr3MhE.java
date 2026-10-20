// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

/** SBR3 lane mh: loop / tryFinally defects with causes (stderr), values on stdout. */
public class Sbr3MhE {
    static int inc(int x) { return x + 1; }
    static boolean lt4(int x) { return x < 4; }
    static int thrower(int x) { throw new IllegalStateException("t3"); }
    static int cleanup(Throwable t, int r, int x) { return -1; }

    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle inc = l.findStatic(Sbr3MhE.class, "inc", MethodType.methodType(int.class, int.class));
        MethodHandle lt4 = l.findStatic(Sbr3MhE.class, "lt4", MethodType.methodType(boolean.class, int.class));
        MethodHandle thr = l.findStatic(Sbr3MhE.class, "thrower", MethodType.methodType(int.class, int.class));
        MethodHandle cl = l.findStatic(Sbr3MhE.class, "cleanup", MethodType.methodType(int.class, Throwable.class, int.class, int.class));
        try {
            System.out.println("tf " + (int) MethodHandles.tryFinally(thr, cl).invoke(3));
        } catch (Throwable t) {
            System.out.println("tf threw " + t);
            for (Throwable c = t; c != null; c = c.getCause()) { c.printStackTrace(System.err); }
        }
        for (int i = 0; i < 40; i++) {
            try {
                int r = (int) MethodHandles.whileLoop(MethodHandles.constant(int.class, 0), lt4, inc).invoke();
                if (i % 10 == 0) System.out.println("while " + i + " " + r);
            } catch (Throwable t) {
                System.out.println("while " + i + " threw " + t);
                for (Throwable c = t; c != null; c = c.getCause()) { c.printStackTrace(System.err); }
                break;
            }
        }
    }
}
