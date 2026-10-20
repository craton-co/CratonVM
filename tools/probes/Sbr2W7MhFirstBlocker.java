// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

/**
 * SBR round 2 wave 7 lane MH: what is the FIRST thing the real
 * {@code java.lang.invoke} stack needs that this VM does not provide? Run with
 * the whole MethodHandle-carrier table retired (the trial build, nothing
 * un-retired) so {@code identity}/{@code insertArguments}/{@code asType}
 * execute the JDK's own bytecode, and print the full cause chain. The chain is
 * the measured entry of the carrier-replacement design page's stage plan.
 */
public class Sbr2W7MhFirstBlocker {
    static int inc(int x) { return x + 1; }

    static void dump(String what, Throwable t) {
        System.out.println("== " + what + ": " + t);
        int d = 0;
        for (Throwable c = t; c != null && d < 6; c = c.getCause(), d++) {
            System.out.println("  cause[" + d + "] " + c);
            StackTraceElement[] st = c.getStackTrace();
            for (int i = 0; i < Math.min(st.length, 14); i++) System.out.println("      at " + st[i]);
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup L = MethodHandles.lookup();
        try {
            System.out.println("identity " + MethodHandles.identity(int.class).invoke(7));
        } catch (Throwable t) { dump("identity", t); }
        try {
            MethodHandle h = L.findStatic(Sbr2W7MhFirstBlocker.class, "inc", MethodType.methodType(int.class, int.class));
            System.out.println("insertArguments " + MethodHandles.insertArguments(h, 0, 4).invoke());
        } catch (Throwable t) { dump("insertArguments", t); }
        try {
            MethodHandle h = L.findStatic(Sbr2W7MhFirstBlocker.class, "inc", MethodType.methodType(int.class, int.class));
            System.out.println("asType " + h.asType(MethodType.methodType(Object.class, Integer.class)).invoke((Object) 4));
        } catch (Throwable t) { dump("asType", t); }
        try {
            MethodHandle h = L.findStatic(Sbr2W7MhFirstBlocker.class, "inc", MethodType.methodType(int.class, int.class));
            System.out.println("direct " + (int) h.invokeExact(4));
        } catch (Throwable t) { dump("direct", t); }
    }
}
