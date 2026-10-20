// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 114, lane L4: the identity answers of
// `MethodHandle.asType` on real JDK handles, which
// `CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ` (default off) answers by reading
// the handle's `asTypeCache` instead of re-entering the JDK's `asType`
// bytecode (page `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`).
// Every line must be the same with the switch off and on, both modes, JIT and
// --nojit; the warm loops exercise the read once the cache holds.
//
// Run: javac -d . L4W114AsTypeCacheRead.java
//      java -cp . L4W114AsTypeCacheRead                                  (HotSpot reference)
//      cratonvm --jdk-only -cp . L4W114AsTypeCacheRead
//      CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ=1 cratonvm --jdk-only -cp . L4W114AsTypeCacheRead
//      (and the same two with --compatible, and with
//      CRATONVM_MH_IWA_JDK_BODY=1 added to the switch-on arm)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// own-type-is-self true
// generic-twice-same true
// generic-type (Object)Object
// generic-invoke 14
// other-type-replaces true
// first-adapter-evicted true
// after-evict-same true
// soft-same true
// soft-type (Box)Object
// soft-invoke Box
// warm-generic-same 3000
// warm-invoke 3000
// warm-iwa 3000
// warm-iwa-list 3000
// null-type java.lang.NullPointerException
// bad-type java.lang.invoke.WrongMethodTypeException
// HOTSPOT_EXPECTED_END

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.List;

public class L4W114AsTypeCacheRead {
    static class Box {
        @Override
        public String toString() {
            return "Box";
        }
    }

    static int twice(int x) {
        return x * 2;
    }

    static Object id(Object o) {
        return o;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W114AsTypeCacheRead.class;
        MethodHandle twice = l.findStatic(me, "twice", MethodType.methodType(int.class, int.class));
        MethodHandle id = l.findStatic(me, "id", MethodType.methodType(Object.class, Object.class));
        MethodType g = MethodType.genericMethodType(1);
        MethodType g2 = MethodType.methodType(Object.class, int.class);

        System.out.println("own-type-is-self " + (twice.asType(twice.type()) == twice));
        MethodHandle a1 = twice.asType(g);
        MethodHandle a2 = twice.asType(g);
        System.out.println("generic-twice-same " + (a1 == a2));
        System.out.println("generic-type " + a1.type());
        System.out.println("generic-invoke " + a1.invokeExact((Object) Integer.valueOf(7)));

        // The strong cache holds one adapter: another type replaces it.
        MethodHandle b = twice.asType(g2);
        System.out.println("other-type-replaces " + (twice.asType(g2) == b));
        MethodHandle a3 = twice.asType(g);
        System.out.println("first-adapter-evicted " + (a3 != a1 && a3 != b));
        System.out.println("after-evict-same " + (twice.asType(g) == a3));

        // An application class in the new type of an all-boot handle: the
        // soft cache (not safe to cache strongly), the JDK body's own arm.
        MethodType soft = MethodType.methodType(Object.class, Box.class);
        MethodHandle s1 = id.asType(soft);
        MethodHandle s2 = id.asType(soft);
        System.out.println("soft-same " + (s1 == s2));
        System.out.println("soft-type " + s1.type().toString().replace(me.getName() + "$", ""));
        System.out.println("soft-invoke " + s1.invoke(new Box()));

        int same = 0;
        int invoked = 0;
        int iwa = 0;
        int iwaList = 0;
        Integer seven = 7;
        List<Object> one = List.of(seven);
        for (int i = 0; i < 3000; i++) {
            if (twice.asType(g) == a3) {
                same++;
            }
            if (((Integer) (Object) a3.invokeExact((Object) seven)) == 14) {
                invoked++;
            }
            if (((Integer) twice.invokeWithArguments(seven)) == 14) {
                iwa++;
            }
            if (((Integer) twice.invokeWithArguments(one)) == 14) {
                iwaList++;
            }
        }
        System.out.println("warm-generic-same " + same);
        System.out.println("warm-invoke " + invoked);
        System.out.println("warm-iwa " + iwa);
        System.out.println("warm-iwa-list " + iwaList);

        try {
            twice.asType(null);
            System.out.println("null-type none");
        } catch (Throwable t) {
            System.out.println("null-type " + t.getClass().getName());
        }
        try {
            twice.asType(MethodType.methodType(int.class, String.class));
            System.out.println("bad-type none");
        } catch (Throwable t) {
            System.out.println("bad-type " + t.getClass().getName());
        }
    }
}
