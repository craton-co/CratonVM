// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;

// The reflective-call rows of docs/known-issues/jit/reflective-calls-trail-hotspot-by-microseconds.md,
// one layer at a time. `ProxySplit <row> [reps] [ops]` prints us/op per rep (10 * ops calls):
//   mhc     static final asType'd MethodHandle, invokeExact      (the compiled constant handle)
//   mhv     the same handle in a local (a non-constant handle)   (the runtime bind hop)
//   invoke  static final Method.invoke on a lambda receiver
//   invcls  static final Method.invoke on a class receiver
//   invacc  invoke after setAccessible(true)                    (no getCallerClass, no checkAccess)
//   prefl   a JDK Proxy whose handler calls Method.invoke       (FrozenRowsBench proxy, one layer)
public class ProxySplit {
    public interface Service { int price(int item); }
    public static final class Plain implements Service { public int price(int item) { return item * 3; } }

    static final Service LAMBDA = item -> item * 3;
    static final Service PLAIN = new Plain();
    static final Method M;
    static final Method MA;
    static final MethodHandle MHC;
    static MethodHandle MHV;
    static {
        try {
            M = Service.class.getMethod("price", int.class);
            MA = Service.class.getMethod("price", int.class);
            MA.setAccessible(true);
            MHC = MethodHandles.lookup().findVirtual(Service.class, "price", MethodType.methodType(int.class, int.class))
                    .asType(MethodType.methodType(Object.class, Object.class, Object.class));
            MHV = MHC;
        } catch (Exception e) {
            throw new Error(e);
        }
    }
    static final InvocationHandler H = (p, m, a) -> m.invoke(LAMBDA, a);
    static final Service PROXY = (Service) Proxy.newProxyInstance(
            ProxySplit.class.getClassLoader(), new Class<?>[] {Service.class}, H);

    static long mhc(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) s += (Integer) (Object) MHC.invokeExact((Object) LAMBDA, (Object) Integer.valueOf(i));
        return s;
    }

    static long mhv(int n) throws Throwable {
        long s = 0;
        MethodHandle h = MHV;
        for (int i = 0; i < n; i++) s += (Integer) (Object) h.invokeExact((Object) LAMBDA, (Object) Integer.valueOf(i));
        return s;
    }

    static long invoke(int n) throws Exception {
        long s = 0;
        for (int i = 0; i < n; i++) s += (Integer) M.invoke(LAMBDA, i);
        return s;
    }

    static long invcls(int n) throws Exception {
        long s = 0;
        for (int i = 0; i < n; i++) s += (Integer) M.invoke(PLAIN, i);
        return s;
    }

    static long invacc(int n) throws Exception {
        long s = 0;
        for (int i = 0; i < n; i++) s += (Integer) MA.invoke(LAMBDA, i);
        return s;
    }

    static long prefl(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += PROXY.price(i);
        return s;
    }

    static long run(String row, int n) throws Throwable {
        switch (row) {
            case "mhc": return mhc(n);
            case "mhv": return mhv(n);
            case "invoke": return invoke(n);
            case "invcls": return invcls(n);
            case "invacc": return invacc(n);
            case "prefl": return prefl(n);
            default: throw new IllegalArgumentException(row);
        }
    }

    public static void main(String[] a) throws Throwable {
        String row = a.length > 0 ? a[0] : "prefl";
        int reps = a.length > 1 ? Integer.parseInt(a[1]) : 5;
        int n = a.length > 2 ? Integer.parseInt(a[2]) : 20000;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            long s = 0;
            for (int k = 0; k < 10; k++) s += run(row, n);
            double us = (System.nanoTime() - t0) / 1000.0 / (10.0 * n);
            System.out.printf("row %s rep%d %.4f us/op %d%n", row, r, us, s);
        }
    }
}
