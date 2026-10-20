// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Proxy;

/**
 * SBR round 2 wave 9 lane MHFIX: cost of invoking a spreader (the arm the lane
 * changed) and of the {@code invokeDefault} path that depends on it. us/op, one
 * line per row; run interleaved A/B, medians of >= 5.
 */
public class Sbr2W9MhfixBench {
    static int sink;
    static int add(int a, int b) { return a + b; }
    static String cat(String a, String b) { return a + b; }

    public interface Iface { default String hello(String s) { return "hi " + s; } }

    interface Body { long run(int n) throws Throwable; }

    static void bench(String name, int n, Body b) throws Throwable {
        b.run(Math.max(1, n / 5));
        long t0 = System.nanoTime();
        long s = b.run(n);
        long t1 = System.nanoTime();
        sink += (int) s;
        System.out.printf(java.util.Locale.ROOT, "bench %s %.4f%n", name, (t1 - t0) / 1000.0 / n);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        MethodHandles.Lookup L = MethodHandles.lookup();
        Class<?> C = Sbr2W9MhfixBench.class;
        MethodHandle ADD = L.findStatic(C, "add", MethodType.methodType(int.class, int.class, int.class));
        MethodHandle CAT = L.findStatic(C, "cat", MethodType.methodType(String.class, String.class, String.class));
        MethodHandle SP_I = ADD.asSpreader(int[].class, 2);
        MethodHandle SP_O = ADD.asSpreader(Object[].class, 2);
        MethodHandle SP_S = CAT.asSpreader(Object[].class, 2);
        MethodHandle SP_P = CAT.asSpreader(1, Object[].class, 1);
        final int[] ia = {3, 9};
        final Object[] oa = {3, 9};
        final Object[] sa = {"a", "b"};
        final Object[] pa = {"b"};
        InvocationHandler h = (p, m, a) -> InvocationHandler.invokeDefault(p, m, a);
        Iface px = (Iface) Proxy.newProxyInstance(C.getClassLoader(), new Class<?>[] {Iface.class}, h);
        for (int r = 0; r < reps; r++) {
            bench("spread.int", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) SP_I.invoke(ia); return s; });
            bench("spread.obj", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += (int) SP_O.invoke(oa); return s; });
            bench("spread.str", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += ((String) SP_S.invoke(sa)).length(); return s; });
            bench("spread.pos1", 20000, n -> { long s = 0; for (int i = 0; i < n; i++) s += ((String) SP_P.invoke("a", pa)).length(); return s; });
            bench("invokeDefault", 5000, n -> { long s = 0; for (int i = 0; i < n; i++) s += px.hello("x").length(); return s; });
        }
        System.out.println("sink " + sink);
    }
}
