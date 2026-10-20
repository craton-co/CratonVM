// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 118, lane L4 (follow-on of
// docs/internal/fixed-bugs/interpreter-i62-L4-proposal-render-an-unnameable-mirror-as-itself-not-object-DONE-20261224.md,
// under `CRATONVM_MH_UNNAMEABLE_MIRROR_OWN_NAME`, default on): the strict
// mirror renderers name a lambda proxy class P by its own descriptor, so an
// EXACT VarHandle whose value type is P (`filterValue` over an `Object[]`
// element handle) is judged at an `(Object[],int)Object` site as HotSpot
// judges it, the access-mode type names P, and the non-exact handle still
// adapts. Both modes, JIT and --nojit; `=0` of the switch is the old answer
// (the proxy erased to Object).
//
// Run: javac -d . L4W118ProxyTypedVarHandle.java
//      java -cp . L4W118ProxyTypedVarHandle                     (HotSpot)
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L4W118ProxyTypedVarHandle
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// varType L4W118ProxyTypedVarHandle$$Lambda
// get-type (Object[],int)L4W118ProxyTypedVarHandle$$Lambda
// plain-get true
// exact true
// exact-erased-get java.lang.invoke.WrongMethodTypeException
// exact-erased-set java.lang.invoke.WrongMethodTypeException
// plain-wrong-set java.lang.ClassCastException
// tomh-type (Object[],int)L4W118ProxyTypedVarHandle$$Lambda
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L4W118ProxyTypedVarHandle {
    static String fold(Object o) {
        return String.valueOf(o).replaceAll("\\$\\$Lambda/0x[0-9a-f]+", "\\$\\$Lambda")
                .replaceAll("\\$\\$Lambda\\$[0-9]+/0x[0-9a-f]+", "\\$\\$Lambda");
    }

    static Object up(Object o) {
        return o;
    }

    public static void main(String[] args) throws Throwable {
        Runnable r = () -> { };
        Class<?> p = r.getClass();
        MethodHandles.Lookup l = MethodHandles.lookup();
        VarHandle base = MethodHandles.arrayElementVarHandle(Object[].class);
        MethodHandle toTarget = l.findStatic(L4W118ProxyTypedVarHandle.class, "up",
                MethodType.methodType(Object.class, Object.class)).asType(MethodType.methodType(Object.class, p));
        MethodHandle fromTarget = l.findStatic(L4W118ProxyTypedVarHandle.class, "up",
                MethodType.methodType(Object.class, Object.class)).asType(MethodType.methodType(p, Object.class));
        VarHandle vh = MethodHandles.filterValue(base, toTarget, fromTarget);
        Object[] arr = {r};
        System.out.println("varType " + fold(vh.varType().getName()));
        System.out.println("get-type " + fold(vh.accessModeType(VarHandle.AccessMode.GET)));
        System.out.println("plain-get " + (vh.get(arr, 0) == r));
        VarHandle exact = vh.withInvokeExactBehavior();
        System.out.println("exact " + exact.hasInvokeExactBehavior());
        try {
            Object got = exact.get(arr, 0);
            System.out.println("exact-erased-get none " + (got == r));
        } catch (Throwable t) {
            System.out.println("exact-erased-get " + t.getClass().getName());
        }
        try {
            exact.set(arr, 0, (Object) r);
            System.out.println("exact-erased-set none");
        } catch (Throwable t) {
            System.out.println("exact-erased-set " + t.getClass().getName());
        }
        try {
            vh.set(arr, 0, (Object) "not-p");
            System.out.println("plain-wrong-set none");
        } catch (Throwable t) {
            System.out.println("plain-wrong-set " + t.getClass().getName());
        }
        MethodHandle viaMh = exact.toMethodHandle(VarHandle.AccessMode.GET);
        System.out.println("tomh-type " + fold(viaMh.type()));
    }
}
