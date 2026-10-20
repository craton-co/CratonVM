// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

/** SBR3 lane mh: the asCollector / asVarargsCollector StackOverflow, with the repeating frames. */
public class Sbr3MhG {
    static String cat(String a, String b) { return a + b; }
    static int add(int a, int b) { return a + b; }
    static int sumInts(int[] xs) { int s = 0; for (int x : xs) s += x; return s; }

    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle add = l.findStatic(Sbr3MhG.class, "add", MethodType.methodType(int.class, int.class, int.class));
        try {
            System.out.println("asCollector " + l.findStatic(Sbr3MhG.class, "sumInts", MethodType.methodType(int.class, int[].class)).asCollector(int[].class, 3).invoke(1, 2, 3));
        } catch (Throwable t) {
            System.out.println("asCollector threw " + t);
            StackTraceElement[] st = t.getStackTrace();
            for (int i = 0; i < Math.min(st.length, 14); i++) System.out.println("  " + st[i]);
        }
        try {
            System.out.println("asSpreader " + add.asSpreader(int[].class, 2).type());
        } catch (Throwable t) {
            System.out.println("asSpreader threw " + t);
        }
        try {
            System.out.println("invokeWithArguments(List) " + add.invokeWithArguments(java.util.List.of(3, 4)));
        } catch (Throwable t) {
            System.out.println("invokeWithArguments(List) threw " + t);
            StackTraceElement[] st = t.getStackTrace();
            for (int i = 0; i < Math.min(st.length, 14); i++) System.out.println("  " + st[i]);
        }
    }
}
