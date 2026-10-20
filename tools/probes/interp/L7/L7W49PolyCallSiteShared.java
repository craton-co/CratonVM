// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L7: the signature-polymorphic call-site
// descriptor channel (`native-api/src/poly_call_site.rs`) hands the consuming
// `MethodHandle.invoke` / `invokeExact` native the site cache's own
// `Arc<str>` on a warm hit (`CRATONVM_POLY_CALL_SITE_SHARED`, proposal
// `interpreter-L7-proposal-the-call-site-descriptor-channel-allocates-per-call-FIXED-20261013`). The
// native must read the same descriptor, taken once, as with a copy: every row
// here depends on the CALL SITE's type, and each shape runs cold first and
// again warm (a first-call-only gap shows as a differing row).
//
//   collect-obj      `invoke((Object) null)` on a varargs collector: the null
//                    is COLLECTED -> "[null]"
//   pass-arr         `invoke((String[]) null)` on the same handle: passed
//                    through -> "null"
//   alternate        both sites interleaved in one loop (a stale descriptor
//                    left armed by one would answer the other)
//   exact-int        `invokeExact` at the handle's own type (int)int
//   exact-wmte       a warm `invokeExact` site handed a handle of another type
//   invoke-widen     `(long) invoke(int, int)` on (int,int)int: the call site's
//                    return widens
//   after            a plain registered native after the loops (nothing armed
//                    may leak into it)
//
// Positive control: `CRATONVM_DBG_FIELD_SITE=1` prints `poly-arm-shared: N`
// at the end of the `[site-cache]` tally: ~10^5 with the switch on, 0 with
// `CRATONVM_POLY_CALL_SITE_SHARED=0` (the rows must be identical either way).
//
// Expected (HotSpot 25.0.3, `java L7W49PolyCallSiteShared`, and `-Xint`):
//   cold collect-obj [null]
//   cold pass-arr null
//   cold exact-int 42
//   cold invoke-widen 7
//   collect-obj 20000
//   pass-arr 20000
//   alternate 20000 20000
//   exact-int 599980000
//   exact-wmte WrongMethodTypeException after 20000: handle's method type (long)long but found (int)int
//   invoke-widen 140000
//   warm collect-obj [null]
//   warm pass-arr null
//   warm exact-int 42
//   warm invoke-widen 7
//   after 3
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.WrongMethodTypeException;
import java.util.Arrays;

public class L7W49PolyCallSiteShared {
    static final int N = 20_000;

    static int twice(int x) {
        return x + x;
    }

    static long twiceLong(long x) {
        return x + x;
    }

    static int max(int a, int b) {
        return Math.max(a, b);
    }

    static MethodHandle arraysToString;
    static MethodHandle twiceMh;
    static MethodHandle twiceLongMh;
    static MethodHandle maxMh;

    static String collectObj() throws Throwable {
        return (String) arraysToString.invoke((Object) null);
    }

    static String passArr() throws Throwable {
        return (String) arraysToString.invoke((String[]) null);
    }

    static int exactInt(MethodHandle mh, int x) throws Throwable {
        return (int) mh.invokeExact(x);
    }

    static long invokeWiden(int a, int b) throws Throwable {
        return (long) maxMh.invoke(a, b);
    }

    static void shapes(String label) throws Throwable {
        System.out.println(label + " collect-obj " + collectObj());
        System.out.println(label + " pass-arr " + passArr());
        System.out.println(label + " exact-int " + exactInt(twiceMh, 21));
        System.out.println(label + " invoke-widen " + invokeWiden(3, 7));
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        arraysToString = l.findStatic(Arrays.class, "toString",
                MethodType.methodType(String.class, Object[].class)).asVarargsCollector(Object[].class);
        twiceMh = l.findStatic(L7W49PolyCallSiteShared.class, "twice",
                MethodType.methodType(int.class, int.class));
        twiceLongMh = l.findStatic(L7W49PolyCallSiteShared.class, "twiceLong",
                MethodType.methodType(long.class, long.class));
        maxMh = l.findStatic(L7W49PolyCallSiteShared.class, "max",
                MethodType.methodType(int.class, int.class, int.class));

        shapes("cold");

        int collected = 0;
        for (int i = 0; i < N; i++) {
            if (collectObj().equals("[null]")) {
                collected++;
            }
        }
        System.out.println("collect-obj " + collected);

        int passed = 0;
        for (int i = 0; i < N; i++) {
            if (passArr().equals("null")) {
                passed++;
            }
        }
        System.out.println("pass-arr " + passed);

        int a = 0;
        int b = 0;
        for (int i = 0; i < N; i++) {
            if (collectObj().equals("[null]")) {
                a++;
            }
            if (passArr().equals("null")) {
                b++;
            }
        }
        System.out.println("alternate " + a + " " + b);

        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += exactInt(twiceMh, i + 5000);
        }
        System.out.println("exact-int " + sum);

        int ok = 0;
        try {
            for (int i = 0; i <= N; i++) {
                exactInt(i < N ? twiceMh : twiceLongMh, i);
                ok++;
            }
            System.out.println("exact-wmte no exception");
        } catch (WrongMethodTypeException e) {
            System.out.println("exact-wmte " + e.getClass().getSimpleName() + " after " + ok + ": " + e.getMessage());
        }

        long widened = 0;
        for (int i = 0; i < N; i++) {
            widened += invokeWiden(3, 7);
        }
        System.out.println("invoke-widen " + widened);

        shapes("warm");

        System.out.println("after " + Integer.parseInt("3"));
    }
}
