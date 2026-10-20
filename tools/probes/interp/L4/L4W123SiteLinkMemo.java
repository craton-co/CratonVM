// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 123, lane L4 (`CRATONVM_MH_SITE_LINK_MEMO`, default
// off, performance; proposal
// docs/internal/proposals/interpreter/i100-L4-proposal-method-handle-warm-call-directions-20261202.md,
// item 4): an interpreted `MethodHandle.invokeExact` / `invoke` site whose
// receiver keeps changing (so the signature-polymorphic route cache never
// records it) links once per thread instead of on every execution.
//
// Rows: the answers of one megamorphic `invokeExact` site and one `invoke`
// site over eight handle shapes of the same type (direct static, virtual,
// bound, `asType`, `insertArguments`, `filterReturnValue`, `guardWithTest`,
// `dropArguments`), whose descriptors name an application class (`Pt`, so the
// link has a user name to resolve and access-check), from two threads; then
// a `WrongMethodTypeException` row (a site whose type is not the handle's: the
// link succeeds, the door refuses, on every execution). TIME rows: ns per
// call of the megamorphic site, the bench half.
//
// Every non-TIME row must be SAME with the switch on and off, both modes,
// `--nojit` (the interpreter's link is the one memoised; under the JIT the
// compiled site keeps its own). With it, run
// L4W81SiteTypeMissingClass, L4W98AppSiteAccess and L4W99SiteTypeUnloadedJdkClass
// with the switch on: SAME (a failed link is never remembered).
// A/B: `cratonvm --compatible --nojit` with and without
// `CRATONVM_MH_SITE_LINK_MEMO=1`, TIME rows, medians of 5 interleaved runs,
// pinned to P-cores.
//
// Run: javac -d . L4W123SiteLinkMemo.java
//      java -cp . L4W123SiteLinkMemo                                   (HotSpot)
//      [CRATONVM_MH_SITE_LINK_MEMO=1] cratonvm --jdk-only|--compatible [--nojit] -cp . L4W123SiteLinkMemo
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; -Xint identical, TIME rows excluded)
// exact 0 (Pt,int)int -> 7
// generic 0 -> 7
// exact 1 (Pt,int)int -> 7
// generic 1 -> 7
// exact 2 (Pt,int)int -> 102
// generic 2 -> 102
// exact 3 (Pt,int)int -> 7
// generic 3 -> 7
// exact 4 (Pt,int)int -> 17
// generic 4 -> 17
// exact 5 (Pt,int)int -> -7
// generic 5 -> -7
// exact 6 (Pt,int)int -> 14
// generic 6 -> 14
// exact 7 (Pt,int)int -> 14
// generic 7 -> 14
// round 88000
// thread-round 88000
// wmte 0 java.lang.invoke.WrongMethodTypeException: handle's method type (Pt,long)int but found (Pt,int)int
// wmte 1 java.lang.invoke.WrongMethodTypeException: handle's method type (Pt,long)int but found (Pt,int)int
// wmte 2 java.lang.invoke.WrongMethodTypeException: handle's method type (Pt,long)int but found (Pt,int)int
// done
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W123SiteLinkMemo {

    static final class Pt {
        final int x;

        Pt(int x) {
            this.x = x;
        }

        int plus(int k) {
            return x + k;
        }
    }

    static int add(Pt p, int k) {
        return p.x + k;
    }

    static int twice(Pt p, int k) {
        return 2 * (p.x + k);
    }

    static int addLong(Pt p, long k) {
        return p.x + (int) k;
    }

    static int three(Object ignored, Pt p, int k) {
        return 3 * p.x + k;
    }

    static int neg(int v) {
        return -v;
    }

    static boolean even(Pt p, int k) {
        return (p.x & 1) == 0;
    }

    static MethodHandle[] shapes() throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType t = MethodType.methodType(int.class, Pt.class, int.class);
        MethodHandle add = l.findStatic(L4W123SiteLinkMemo.class, "add", t);
        MethodHandle twice = l.findStatic(L4W123SiteLinkMemo.class, "twice", t);
        MethodHandle plus = l.findVirtual(Pt.class, "plus", MethodType.methodType(int.class, int.class));
        MethodHandle bound = MethodHandles.dropArguments(
                MethodHandles.insertArguments(add, 0, new Pt(100)), 0, Pt.class);
        MethodHandle asType = l.findStatic(L4W123SiteLinkMemo.class, "addLong",
                MethodType.methodType(int.class, Pt.class, long.class)).asType(t);
        MethodHandle three = MethodHandles.insertArguments(
                l.findStatic(L4W123SiteLinkMemo.class, "three", MethodType.methodType(int.class, Object.class, Pt.class, int.class)), 0, "x");
        MethodHandle filtered = MethodHandles.filterReturnValue(add,
                l.findStatic(L4W123SiteLinkMemo.class, "neg", MethodType.methodType(int.class, int.class)));
        MethodHandle guard = MethodHandles.guardWithTest(
                l.findStatic(L4W123SiteLinkMemo.class, "even", MethodType.methodType(boolean.class, Pt.class, int.class)),
                add, twice);
        return new MethodHandle[] {add, plus, bound, asType, three, filtered, guard, twice};
    }

    static int exactSite(MethodHandle mh, Pt p, int k) throws Throwable {
        return (int) mh.invokeExact(p, k);
    }

    static Object genericSite(MethodHandle mh, Pt p, int k) throws Throwable {
        return mh.invoke(p, k);
    }

    static long round(MethodHandle[] hs, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += exactSite(hs[i & 7], new Pt(i & 15), i & 3);
        }
        return sum;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandle[] hs = shapes();
        Pt p = new Pt(5);
        for (int i = 0; i < hs.length; i++) {
            System.out.println("exact " + i + " " + hs[i].type() + " -> " + exactSite(hs[i], p, 2));
            System.out.println("generic " + i + " -> " + genericSite(hs[i], p, 2));
        }
        System.out.println("round " + round(hs, 4000));
        long[] other = new long[1];
        Thread t = new Thread(() -> {
            try {
                other[0] = round(hs, 4000);
            } catch (Throwable e) {
                other[0] = -1;
            }
        });
        t.start();
        t.join();
        System.out.println("thread-round " + other[0]);
        MethodHandle wide = MethodHandles.lookup().findStatic(L4W123SiteLinkMemo.class, "addLong",
                MethodType.methodType(int.class, Pt.class, long.class));
        for (int i = 0; i < 3; i++) {
            try {
                System.out.println("wmte " + i + " " + exactSite(wide, p, 1));
            } catch (Throwable e) {
                System.out.println("wmte " + i + " " + e.getClass().getName() + ": " + e.getMessage());
            }
        }
        round(hs, 20000);
        for (int rep = 0; rep < 3; rep++) {
            long t0 = System.nanoTime();
            long s = round(hs, 200000);
            long dt = System.nanoTime() - t0;
            System.out.println("TIME megamorphic-exact rep " + rep + " " + (dt / 200000) + " ns/call (sum " + s + ")");
        }
        System.out.println("done");
    }
}
