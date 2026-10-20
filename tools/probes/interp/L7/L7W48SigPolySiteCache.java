// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L7: the interpreter's per-site route cache
// for signature-polymorphic `invokevirtual` sites (`VarHandle` access modes,
// `MethodHandle.invokeExact` / `invoke`; `CRATONVM_SIG_POLY_SITE_CACHE`,
// `vm/src/runtime/interpreter/sig_poly_site.rs`; proposal
// `interpreter-L7-proposal-cache-signature-polymorphic-invokes-in-the-interpreter-FIXED-20261012`).
// The cache holds the ROUTE only (which native, which call-site channels to
// arm), guarded by the receiver handle's class; every per-call rule stays in
// the native. Each row warms ONE call site, then hands it a case the route
// must not answer by itself:
//
//   vh-get / vh-set / vh-cas / vh-gaa-long   warm access-mode sites
//   vh-poly           one site, two VarHandle classes (read-write, read-only)
//   vh-exact          a warm site handed an invoke-exact VarHandle whose
//                     access mode type is not the site's -> WMTE
//   vh-null-handle    a warm site handed a null VarHandle -> NPE
//   vh-null-coord     a warm site handed a null coordinate -> NPE
//   mh-exact          a warm invokeExact site
//   mh-poly           one invokeExact site, three handle shapes (direct,
//                     bound, filtered)
//   mh-exact-wmte     a warm invokeExact site handed a handle of another type
//   mh-null           a warm invokeExact site handed a null handle -> NPE
//   mh-invoke-null    `invoke` on a varargs collector: the trailing null is
//                     collected or passed through by the SITE's static type
//   mcs               a MutableCallSite's dynamicInvoker, retargeted between
//                     loops, and a VolatileCallSite retargeted inside one
//
// Positive control: `CRATONVM_DBG_INVOKESTATS=1` prints a `sigpoly_hit=`
// counter that must be non-zero with the switch on and zero with
// `CRATONVM_SIG_POLY_SITE_CACHE=0`.
//
// Expected (HotSpot 25, `java L7W48SigPolySiteCache`, and `-Xint`):
//   vh-get 199990000
//   vh-set 199990000
//   vh-cas 20000
//   vh-gaa-long 20000
//   vh-poly 50000
//   vh-exact WrongMethodTypeException after 20000
//   vh-null-handle NullPointerException after 20000
//   vh-null-coord NullPointerException after 20000
//   mh-exact 200010000
//   mh-poly 1400130000
//   mh-exact-wmte WrongMethodTypeException after 20000
//   mh-null NullPointerException after 20000
//   mh-invoke-null [x] null [null] null
//   mcs 200010000 399980000
//   vcs 1399990000
//   done
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;
import java.lang.invoke.VarHandle;
import java.lang.invoke.VolatileCallSite;
import java.util.Arrays;

public class L7W48SigPolySiteCache {
    static final int N = 20_000;

    static final class H {
        int i;
        final int f;
        long l;

        H(int f) {
            this.f = f;
        }
    }

    static final VarHandle I;
    static final VarHandle F;
    static final VarHandle L;

    static {
        try {
            MethodHandles.Lookup lk = MethodHandles.lookup();
            I = lk.findVarHandle(H.class, "i", int.class);
            F = lk.findVarHandle(H.class, "f", int.class);
            L = lk.findVarHandle(H.class, "l", long.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static int inc(int x) {
        return x + 1;
    }

    static int twice(int x) {
        return x * 2;
    }

    static int add(int a, int b) {
        return a + b;
    }

    static int boxedInc(Integer x) {
        return x + 1;
    }

    static int third(int x) {
        return x / 3;
    }

    // ---- the call sites (one per helper) ----------------------------------

    static int vhGet(VarHandle vh, H h) {
        return (int) vh.get(h);
    }

    static long vhGetAsLong(VarHandle vh, H h) {
        return (long) vh.get(h);
    }

    static int mhExact(MethodHandle mh, int x) throws Throwable {
        return (int) mh.invokeExact(x);
    }

    static String name(Throwable t) {
        return t.getClass().getSimpleName();
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        H h = new H(5);

        long s = 0;
        for (int k = 0; k < N; k++) {
            h.i = k;
            s += vhGet(I, h);
        }
        System.out.println("vh-get " + s);

        s = 0;
        for (int k = 0; k < N; k++) {
            I.set(h, k);
            s += h.i;
        }
        System.out.println("vh-set " + s);

        s = 0;
        h.i = 0;
        for (int k = 0; k < N; k++) {
            if (I.compareAndSet(h, k, k + 1)) {
                s++;
            }
        }
        System.out.println("vh-cas " + s);

        h.l = 0;
        for (int k = 0; k < N; k++) {
            long unused = (long) L.getAndAdd(h, 1L);
        }
        System.out.println("vh-gaa-long " + h.l);

        s = 0;
        h.i = 0;
        for (int k = 0; k < N; k++) {
            s += vhGet((k & 1) == 0 ? I : F, h);
        }
        System.out.println("vh-poly " + s);

        int done = 0;
        try {
            for (int k = 0; k < N; k++) {
                vhGetAsLong(I, h);
                done++;
            }
            vhGetAsLong(I.withInvokeExactBehavior(), h);
            System.out.println("vh-exact no exception");
        } catch (Throwable t) {
            System.out.println("vh-exact " + name(t) + " after " + done);
        }

        done = 0;
        try {
            for (int k = 0; k < N; k++) {
                vhGet(I, h);
                done++;
            }
            vhGet(null, h);
            System.out.println("vh-null-handle no exception");
        } catch (Throwable t) {
            System.out.println("vh-null-handle " + name(t) + " after " + done);
        }

        done = 0;
        try {
            for (int k = 0; k < N; k++) {
                vhGet(I, h);
                done++;
            }
            vhGet(I, null);
            System.out.println("vh-null-coord no exception");
        } catch (Throwable t) {
            System.out.println("vh-null-coord " + name(t) + " after " + done);
        }

        MethodType ii = MethodType.methodType(int.class, int.class);
        MethodHandle incH = lk.findStatic(L7W48SigPolySiteCache.class, "inc", ii);
        MethodHandle twiceH = lk.findStatic(L7W48SigPolySiteCache.class, "twice", ii);
        MethodHandle addH = lk.findStatic(L7W48SigPolySiteCache.class, "add",
                MethodType.methodType(int.class, int.class, int.class));
        MethodHandle bound = MethodHandles.insertArguments(addH, 0, 7);
        MethodHandle filtered = MethodHandles.filterReturnValue(incH,
                lk.findStatic(L7W48SigPolySiteCache.class, "third", ii));

        s = 0;
        for (int k = 0; k < N; k++) {
            s += mhExact(incH, k);
        }
        System.out.println("mh-exact " + s);

        s = 0;
        MethodHandle[] shapes = {incH, bound, filtered};
        for (int k = 0; k < 3 * N; k++) {
            s += mhExact(shapes[k % 3], k);
        }
        System.out.println("mh-poly " + s);

        done = 0;
        try {
            for (int k = 0; k < N; k++) {
                mhExact(incH, k);
                done++;
            }
            MethodHandle boxed = lk.findStatic(L7W48SigPolySiteCache.class, "boxedInc",
                    MethodType.methodType(int.class, Integer.class));
            mhExact(boxed, 1);
            System.out.println("mh-exact-wmte no exception");
        } catch (Throwable t) {
            System.out.println("mh-exact-wmte " + name(t) + " after " + done);
        }

        done = 0;
        try {
            for (int k = 0; k < N; k++) {
                mhExact(incH, k);
                done++;
            }
            mhExact(null, 1);
            System.out.println("mh-null no exception");
        } catch (Throwable t) {
            System.out.println("mh-null " + name(t) + " after " + done);
        }

        MethodHandle toStr = lk.findStatic(Arrays.class, "toString",
                MethodType.methodType(String.class, Object[].class)).asVarargsCollector(Object[].class);
        String a = null;
        String b = null;
        String c = null;
        String d = null;
        for (int k = 0; k < N; k++) {
            a = (String) toStr.invoke((Object) "x");
            b = (String) toStr.invoke((Object[]) null);
        }
        for (int k = 0; k < N; k++) {
            c = (String) toStr.invoke((Object) null);
            d = (String) toStr.invoke((Object[]) null);
        }
        System.out.println("mh-invoke-null " + a + " " + b + " " + c + " " + d);

        MutableCallSite mcs = new MutableCallSite(ii);
        mcs.setTarget(incH);
        MethodHandle dyn = mcs.dynamicInvoker();
        long m1 = 0;
        for (int k = 0; k < N; k++) {
            m1 += (int) dyn.invokeExact(k);
        }
        mcs.setTarget(twiceH);
        long m2 = 0;
        for (int k = 0; k < N; k++) {
            m2 += (int) dyn.invokeExact(k);
        }
        System.out.println("mcs " + m1 + " " + m2);

        VolatileCallSite vcs = new VolatileCallSite(incH);
        MethodHandle vdyn = vcs.dynamicInvoker();
        long v = 0;
        for (int k = 0; k < 2 * N; k++) {
            if (k == N) {
                vcs.setTarget(twiceH);
            }
            v += (int) vdyn.invokeExact(k);
        }
        System.out.println("vcs " + v);
        System.out.println("done");
    }
}
