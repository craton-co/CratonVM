// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 93, lane L4 (proposal
// i62-L4-proposal-render-an-unnameable-mirror-as-itself-not-object, Wave 92
// remainder): a lambda proxy class P under `explicitCastArguments`, and the
// rows that prove wave 62's per-site patch in the carrier `invokeExact` check
// (`exact_call_site_refusal`'s `mh_lambda_proxy_owner` arm) is no longer
// needed now that P renders as itself.
//
// `explicitCastArguments` passes a reference to an INTERFACE uncast but CASTS
// to a class (`MethodHandleImpl.valueConversion`: `VerifyType.isNullConversion
// (src, dst, keepInterfaces=false)`, else `castReference`). P is a final
// hidden class, so a `String` must be refused. The carrier model skipped any
// target `class_id_by_name` could not resolve under the explicit scope
// (`leaf_reference_refusal`), and P is one, so the `String` passed. The
// `explicit-iface-uncast` row is the control the fix must not move.
//
// Run: javac -d . L4W93ExplicitCastToProxy.java
//      java -cp . L4W93ExplicitCastToProxy
//      cratonvm --compatible -cp . L4W93ExplicitCastToProxy
//      cratonvm --jdk-only -cp . L4W93ExplicitCastToProxy
//      CRATONVM_MH_UNNAMEABLE_MIRROR_OWN_NAME=0 cratonvm --compatible -cp . L4W93ExplicitCastToProxy
//        (kill switch: the explicit-*-wrong, bindTo-wrong and exact-* rows
//        return to the pre-wave-92 answers)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, derived from the JDK 25 sources)
// explicit-type: (Object)Object
// explicit-param-right: true
// explicit-param-wrong: java.lang.ClassCastException: Cannot cast java.lang.String to P
// explicit-return-right: true
// explicit-return-wrong: java.lang.ClassCastException: Cannot cast java.lang.String to P
// explicit-iface-uncast: x
// explicit-param-null: null
// bindTo-wrong: java.lang.ClassCastException: Cannot cast java.lang.String to P
// exact-erased: java.lang.invoke.WrongMethodTypeException: handle's method type (P)P but found (Object)Object
// exact-erased-int: java.lang.invoke.WrongMethodTypeException: handle's method type (P,int)int but found (Object,int)int
// exact-generic-right: true
// warm: 2000
// again-explicit-param-wrong: java.lang.ClassCastException: Cannot cast java.lang.String to P
// again-explicit-return-wrong: java.lang.ClassCastException: Cannot cast java.lang.String to P
// again-explicit-iface-uncast: x
// again-exact-erased: java.lang.invoke.WrongMethodTypeException: handle's method type (P)P but found (Object)Object
// again-exact-erased-int: java.lang.invoke.WrongMethodTypeException: handle's method type (P,int)int but found (Object,int)int
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.IntUnaryOperator;

public class L4W93ExplicitCastToProxy {
    interface Row {
        Object run() throws Throwable;
    }

    static String mask = "\0";

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        System.out.println(name + ": " + out.replace(mask, "P"));
    }

    public static void main(String[] args) throws Throwable {
        IntUnaryOperator f = x -> x + 1;
        Class<?> p = f.getClass();
        mask = p.getName();
        MethodType oo = MethodType.methodType(Object.class, Object.class);
        MethodHandle id = MethodHandles.identity(p);
        MethodHandle paramP = MethodHandles.explicitCastArguments(id, oo);
        MethodHandle returnP = MethodHandles.explicitCastArguments(
                MethodHandles.identity(Object.class), MethodType.methodType(p, Object.class));
        MethodHandle iface = MethodHandles.explicitCastArguments(
                MethodHandles.identity(IntUnaryOperator.class), oo);
        // `applyAsInt` of P, receiver typed P: `(P,int)int`.
        MethodHandle apply = MethodHandles.lookup().findVirtual(IntUnaryOperator.class,
                "applyAsInt", MethodType.methodType(int.class, int.class))
                .asType(MethodType.methodType(int.class, p, int.class));
        row("explicit-type", () -> paramP.type());
        row("explicit-param-right", () -> paramP.invoke((Object) f) == f);
        row("explicit-param-wrong", () -> {
            Object o = paramP.invoke((Object) "x");
            return "ran";
        });
        row("explicit-return-right", () -> returnP.invoke((Object) f) == f);
        row("explicit-return-wrong", () -> {
            Object o = returnP.invoke((Object) "x");
            return "ran";
        });
        row("explicit-iface-uncast", () -> iface.invoke((Object) "x"));
        row("explicit-param-null", () -> paramP.invoke((Object) null));
        row("bindTo-wrong", () -> id.bindTo("x"));
        row("exact-erased", () -> {
            Object o = (Object) id.invokeExact((Object) f);
            return "ran";
        });
        row("exact-erased-int", () -> {
            int v = (int) apply.invokeExact((Object) f, 1);
            return "ran";
        });
        row("exact-generic-right", () -> (Object) paramP.invokeExact((Object) f) == f);
        int n = 0;
        for (int i = 0; i < 2000; i++) {
            if (paramP.invoke((Object) f) == f && returnP.invoke((Object) f) == f) {
                n++;
            }
        }
        System.out.println("warm: " + n);
        row("again-explicit-param-wrong", () -> {
            Object o = paramP.invoke((Object) "y");
            return "ran";
        });
        row("again-explicit-return-wrong", () -> {
            Object o = returnP.invoke((Object) "y");
            return "ran";
        });
        row("again-explicit-iface-uncast", () -> iface.invoke((Object) "x"));
        row("again-exact-erased", () -> {
            Object o = (Object) id.invokeExact((Object) f);
            return "ran";
        });
        row("again-exact-erased-int", () -> {
            int v = (int) apply.invokeExact((Object) f, 1);
            return "ran";
        });
    }
}
