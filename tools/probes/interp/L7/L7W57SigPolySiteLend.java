// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L7: the signature-polymorphic route
// cache's hit LENDS its site for the call instead of cloning the `Arc`
// (`CRATONVM_SIG_POLY_SITE_LEND`, `sig_poly_site::SigPolySiteCache::lend`;
// proposal
// `interpreter-L7-proposal-the-signature-polymorphic-site-hit-lends-its-site-instead-of-cloning-the-arc-FIXED-20261021`).
// A performance switch: every line must be the same with it on and off.
// Each shape runs cold (r0) and warm (r1..r3):
//
//   vh      `VarHandle.get` / `set` / `getAndAdd` / `compareAndSet` /
//           `compareAndExchange` of an instance `int` and a `long` field
//   ref     `VarHandle.get` / `set` of a `String` field (narrow reference
//           return) and an `Object[]` element
//   fact    a RE-ENTRANT site: `fact(n)` calls itself through ONE
//           `MethodHandle.invokeExact` site, so the outer hit's native runs
//           Java that reaches the same site while it is lent (the nested hit
//           must decline to the slow path and the answer must not change)
//   npe     a null receiver coordinate at a warm `VarHandle.get` site (the
//           slow path owns the NPE)
//   wmt     `invokeExact` with the wrong type at a warm site (the native owns
//           the `WrongMethodTypeException`)
//   loop    200000 executions of the `vh` int sites and 2000 of `fact(10)`
//
// Positive control: `CRATONVM_DBG_FIELD_SITE=1`, FINAL `[site-cache]` line,
// `sig-poly-lent-hit: N` with N in the hundreds of thousands with
// `CRATONVM_SIG_POLY_SITE_LEND=1` (or default on after the lane's switch-on
// commit) and `sig-poly-lent-hit: 0` with `CRATONVM_SIG_POLY_SITE_LEND=0`;
// `sig-poly-first: hit=` must stay of the same order either way.
//
// Expected (HotSpot 25.0.3, `java L7W57SigPolySiteLend`, and `-Xint`):
//   r0 vh 7 7 8 true 10 false 10 13 8000000003 true
//   r0 ref a b c true
//   r0 fact 1 2 6 3628800 479001600
//   r0 npe java.lang.NullPointerException
//   r0 wmt java.lang.invoke.WrongMethodTypeException
//   r1 vh 7 7 8 true 10 false 10 13 8000000003 true
//   r1 ref a b c true
//   r1 fact 1 2 6 3628800 479001600
//   r1 npe java.lang.NullPointerException
//   r1 wmt java.lang.invoke.WrongMethodTypeException
//   r2 vh 7 7 8 true 10 false 10 13 8000000003 true
//   r2 ref a b c true
//   r2 fact 1 2 6 3628800 479001600
//   r2 npe java.lang.NullPointerException
//   r2 wmt java.lang.invoke.WrongMethodTypeException
//   r3 vh 7 7 8 true 10 false 10 13 8000000003 true
//   r3 ref a b c true
//   r3 fact 1 2 6 3628800 479001600
//   r3 npe java.lang.NullPointerException
//   r3 wmt java.lang.invoke.WrongMethodTypeException
//   loop 400000 7257600000
//   after r0 fact 1 2 6 3628800 479001600
//   after r0 vh 7 7 8 true 10 false 10 13 8000000003 true

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L7W57SigPolySiteLend {
    static final class Box {
        int i;
        long l;
        String s;
    }

    static final VarHandle I;
    static final VarHandle L;
    static final VarHandle S;
    static final VarHandle ARR = MethodHandles.arrayElementVarHandle(Object[].class);
    static final MethodHandle FACT;

    static {
        try {
            MethodHandles.Lookup lk = MethodHandles.lookup();
            I = lk.findVarHandle(Box.class, "i", int.class);
            L = lk.findVarHandle(Box.class, "l", long.class);
            S = lk.findVarHandle(Box.class, "s", String.class);
            FACT = lk.findStatic(L7W57SigPolySiteLend.class, "fact",
                    MethodType.methodType(long.class, int.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    // ONE invokeExact site, re-entered from inside its own call.
    static long fact(int n) throws Throwable {
        if (n <= 1) {
            return 1L;
        }
        return n * (long) FACT.invokeExact(n - 1);
    }

    static String vhLine() {
        Box b = new Box();
        I.set(b, 7);
        int g = (int) I.get(b);
        int old = (int) I.getAndAdd(b, 1);
        int now = (int) I.get(b);
        boolean cas1 = I.compareAndSet(b, 8, 10);
        int afterCas = (int) I.get(b);
        boolean cas2 = I.compareAndSet(b, 8, 11);
        int cae = (int) I.compareAndExchange(b, 10, 13);
        int after = (int) I.get(b);
        L.set(b, 8_000_000_000L);
        L.getAndAdd(b, 3L);
        long lv = (long) L.get(b);
        boolean lcas = L.compareAndSet(b, 8_000_000_003L, 1L);
        return g + " " + old + " " + now + " " + cas1 + " " + afterCas + " " + cas2 + " " + cae
                + " " + after + " " + lv + " " + lcas;
    }

    static String refLine() {
        Box b = new Box();
        S.set(b, "a");
        String s1 = (String) S.get(b);
        S.set(b, "b");
        String s2 = (String) S.get(b);
        Object[] arr = new Object[2];
        ARR.set(arr, 1, "c");
        Object e = ARR.get(arr, 1);
        boolean cas = S.compareAndSet(b, "b", "x");
        return s1 + " " + s2 + " " + e + " " + cas;
    }

    static String factLine() throws Throwable {
        return fact(1) + " " + fact(2) + " " + fact(3) + " " + fact(10) + " " + fact(12);
    }

    static String npeLine() {
        Box nb = null;
        try {
            int v = (int) I.get(nb);
            return "no exception " + v;
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static String wmtLine() {
        try {
            int v = (int) FACT.invokeExact(3);
            return "no exception " + v;
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    public static void main(String[] args) throws Throwable {
        for (int r = 0; r < 4; r++) {
            System.out.println("r" + r + " vh " + vhLine());
            System.out.println("r" + r + " ref " + refLine());
            System.out.println("r" + r + " fact " + factLine());
            System.out.println("r" + r + " npe " + npeLine());
            System.out.println("r" + r + " wmt " + wmtLine());
        }
        Box b = new Box();
        long sum = 0L;
        for (int k = 0; k < 200_000; k++) {
            I.set(b, k);
            sum += (int) I.get(b) >= 0 ? 2L : 0L;
        }
        long f = 0L;
        for (int k = 0; k < 2000; k++) {
            f += fact(10);
        }
        System.out.println("loop " + sum + " " + f);
        System.out.println("after r0 fact " + factLine());
        System.out.println("after r0 vh " + vhLine());
    }
}
