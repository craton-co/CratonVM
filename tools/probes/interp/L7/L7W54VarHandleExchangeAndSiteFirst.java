// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L7: two performance switches of the
// interpreter's signature-polymorphic route cache (`sig_poly_site`), held to
// HotSpot's answers.
//
// * `CRATONVM_VH_INTERP_CAE_FAST`: `VarHandle.compareAndExchange` (plain,
//   `Acquire`, `Release`) over an `int` / `long` instance field is one
//   hardware compare-exchange whose witness is pushed. Each mode runs cold
//   (r0) and warm (r1..r3) at one site per mode and kind, succeeding (witness
//   = expected) and failing (witness = the field), then the refusals the arm
//   must leave to the native at the same sites: a null receiver (NPE), a
//   receiver of another class (ClassCastException), a read-only handle on a
//   final field (UnsupportedOperationException); and a reference exchange
//   site (not served by the arm: the control).
// * `CRATONVM_SIG_POLY_SITE_FIRST`: `invokevirtual` probes the route cache
//   before its fast door. The `mix` rows interleave VarHandle `get` / `set`,
//   `MethodHandle.invokeExact` and ordinary virtual calls (bimorphic) at
//   adjacent pcs of one method, so a filter false positive must fall back to
//   the ordinary call.
//
// Only exception class names are printed (the message texts are lane L4's).
//
// Positive controls (`--nojit`, `CRATONVM_DBG_FIELD_SITE=1`, FINAL line):
// `vh-cae-fast: N` with N at least 600000 with `CRATONVM_VH_INTERP_CAE_FAST=1`
// and 0 without; `sig-poly-first: hit=N` with N in the millions with
// `CRATONVM_SIG_POLY_SITE_FIRST=1` and `hit=0 decline=0` without.
//
// Expected (HotSpot 25.0.3, `java L7W54VarHandleExchangeAndSiteFirst`, and
// `-Xint`):
//   r0 int 4 5 6 6 7 8 8 9 10
//   r0 long 4 5 6 6 7 8 8 9 10
//   r0 ref a b b true
//   r0 refuse java.lang.NullPointerException java.lang.ClassCastException java.lang.UnsupportedOperationException java.lang.NullPointerException
//   r0 mix 21 15 1
//   r1 int 4 5 6 6 7 8 8 9 10
//   r1 long 4 5 6 6 7 8 8 9 10
//   r1 ref a b b true
//   r1 refuse java.lang.NullPointerException java.lang.ClassCastException java.lang.UnsupportedOperationException java.lang.NullPointerException
//   r1 mix 21 15 1
//   r2 int 4 5 6 6 7 8 8 9 10
//   r2 long 4 5 6 6 7 8 8 9 10
//   r2 ref a b b true
//   r2 refuse java.lang.NullPointerException java.lang.ClassCastException java.lang.UnsupportedOperationException java.lang.NullPointerException
//   r2 mix 21 15 1
//   r3 int 4 5 6 6 7 8 8 9 10
//   r3 long 4 5 6 6 7 8 8 9 10
//   r3 ref a b b true
//   r3 refuse java.lang.NullPointerException java.lang.ClassCastException java.lang.UnsupportedOperationException java.lang.NullPointerException
//   r3 mix 21 15 1
//   loop 600000 800000 200000
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L7W54VarHandleExchangeAndSiteFirst {
    static final class H {
        int i;
        long j;
        String s;
        final int fin = 1;
    }

    static final class Other {
        int i;
    }

    static final VarHandle I;
    static final VarHandle J;
    static final VarHandle S;
    static final VarHandle FIN;
    static final MethodHandle TWICE;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            I = l.findVarHandle(H.class, "i", int.class);
            J = l.findVarHandle(H.class, "j", long.class);
            S = l.findVarHandle(H.class, "s", String.class);
            FIN = l.findVarHandle(H.class, "fin", int.class);
            TWICE = l.findStatic(L7W54VarHandleExchangeAndSiteFirst.class, "twice",
                    MethodType.methodType(int.class, int.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static int twice(int x) { return 2 * x; }

    // One site per mode and kind; `Object h` so a foreign receiver reaches it.
    static int xI(Object h, int a, int b) { return (int) I.compareAndExchange(h, a, b); }
    static int xIAcq(Object h, int a, int b) { return (int) I.compareAndExchangeAcquire(h, a, b); }
    static int xIRel(Object h, int a, int b) { return (int) I.compareAndExchangeRelease(h, a, b); }

    static long xJ(H h, long a, long b) { return (long) J.compareAndExchange(h, a, b); }
    static long xJAcq(H h, long a, long b) { return (long) J.compareAndExchangeAcquire(h, a, b); }
    static long xJRel(H h, long a, long b) { return (long) J.compareAndExchangeRelease(h, a, b); }

    static String xS(H h, String a, String b) { return (String) S.compareAndExchange(h, a, b); }

    static int xFin(H h, int a, int b) { return (int) FIN.compareAndExchange(h, a, b); }

    interface IntX { int x(H h, int a, int b); }
    interface LongX { long x(H h, long a, long b); }

    abstract static class Shape { abstract int area(); }
    static final class Sq extends Shape { int area() { return 4; } }
    static final class Tri extends Shape { int area() { return 7; } }

    static String name(Throwable t) {
        return t.getClass().getName();
    }

    // VarHandle, invokeExact and ordinary virtual calls at adjacent pcs.
    static long mix(H h, Shape a, Shape b, int k) throws Throwable {
        long acc = 0;
        I.set(h, k);
        acc += (int) I.get(h);
        acc += a.area();
        acc += (int) TWICE.invokeExact(k);
        acc += b.area();
        acc += (long) J.getVolatile(h);
        return acc;
    }

    static void round(int r) throws Throwable {
        H h = new H();
        StringBuilder sb = new StringBuilder("r" + r + " int");
        h.i = 4;
        IntX[] ints = {
            (x, a, b) -> xI(x, a, b),
            (x, a, b) -> xIAcq(x, a, b),
            (x, a, b) -> xIRel(x, a, b),
        };
        for (IntX c : ints) {
            int before = h.i;
            sb.append(' ').append(c.x(h, before, before + 1));
            sb.append(' ').append(c.x(h, before, before + 100));
            h.i = h.i + 1;
            sb.append(' ').append(h.i);
        }
        System.out.println(sb);

        sb = new StringBuilder("r" + r + " long");
        h.j = 4L;
        LongX[] longs = {
            (x, a, b) -> xJ(x, a, b),
            (x, a, b) -> xJAcq(x, a, b),
            (x, a, b) -> xJRel(x, a, b),
        };
        for (LongX c : longs) {
            long before = h.j;
            sb.append(' ').append(c.x(h, before, before + 1));
            sb.append(' ').append(c.x(h, before, before + 100));
            h.j = h.j + 1;
            sb.append(' ').append(h.j);
        }
        System.out.println(sb);

        h.s = "a";
        String w1 = xS(h, "a", "b");
        String w2 = xS(h, "a", "c");
        String w3 = xS(h, "b", "c");
        System.out.println("r" + r + " ref " + w1 + " " + w2 + " " + w3 + " " + (h.s == "c"));

        String npe;
        try {
            xI(null, 0, 1);
            npe = "none";
        } catch (Throwable t) {
            npe = name(t);
        }
        String cce;
        try {
            xI(new Other(), 0, 1);
            cce = "none";
        } catch (Throwable t) {
            cce = name(t);
        }
        String uoe;
        try {
            xFin(h, 1, 2);
            uoe = "none";
        } catch (Throwable t) {
            uoe = name(t);
        }
        String npeJ;
        try {
            xJ(null, 0, 1);
            npeJ = "none";
        } catch (Throwable t) {
            npeJ = name(t);
        }
        System.out.println("r" + r + " refuse " + npe + " " + cce + " " + uoe + " " + npeJ);

        H m = new H();
        m.j = 1;
        Shape sq = new Sq();
        Shape tri = new Tri();
        long a = mix(m, sq, tri, 3); // 3 + 4 + 6 + 7 + 1 = 21
        long b = mix(m, tri, sq, 1); // 1 + 7 + 2 + 4 + 1 = 15
        System.out.println("r" + r + " mix " + a + " " + b + " " + m.i);
    }

    public static void main(String[] args) throws Throwable {
        for (int r = 0; r < 4; r++) {
            round(r);
        }
        H h = new H();
        long n = 0;
        h.i = 0;
        for (int k = 0; k < 200_000; k++) {
            if (xI(h, k, k + 1) == k) n++;
            if (xIAcq(h, k + 1, k + 1) == k + 1) n++;
            if (xIRel(h, k + 1, k + 1) == k + 1) n++;
        }
        long m = 0;
        h.j = 0;
        for (int k = 0; k < 400_000; k++) {
            if (xJ(h, k, k + 1) == k) m++;
            if (xJRel(h, k + 99, 0) == k + 1) m++;
        }
        long p = 0;
        Shape sq = new Sq();
        Shape tri = new Tri();
        H g = new H();
        for (int k = 0; k < 200_000; k++) {
            if (mix(g, sq, tri, k) == (long) k + 4 + 2L * k + 7) p++;
        }
        System.out.println("loop " + n + " " + m + " " + p);
    }
}
