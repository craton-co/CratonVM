// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L4 (proposal
// i62-L4-proposal-render-an-unnameable-mirror-as-itself-not-object): a lambda
// proxy class P as a PARAMETER and RETURN type of a method handle (wave 62
// covered P as a receiver only). The method-handle model rendered P's mirror
// as `Object` (`mirror_to_descriptor`, `--compatible`) or not at all
// (`link_to_type_descriptor`, `--jdk-only` real-handle doors), so
// `identity(P)` was typed `(Object)Object`, an `(Object)Object` exact call
// site passed, and a `String` passed its `P` cast. Fixed by
// `unnameable_mirror_descriptor` (native-builtins/src/lang_invoke.rs),
// CRATONVM_MH_UNNAMEABLE_MIRROR_OWN_NAME (default on, =0 off). P's name
// (hidden, run-specific) is masked as `P`.
//
// Census: CRATONVM_DBG=mh-finder-census prints `type-render.mirror.*` /
// `type-render.link.*` event rows (proxy-own-name against object-fallback /
// none) at exit.
//
// Run: javac -d . L4W92UnnameableMirror.java
//      java -cp . L4W92UnnameableMirror
//      cratonvm --compatible -cp . L4W92UnnameableMirror
//      cratonvm --jdk-only -cp . L4W92UnnameableMirror
//      CRATONVM_MH_UNNAMEABLE_MIRROR_OWN_NAME=0 cratonvm --compatible -cp . L4W92UnnameableMirror
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, derived from the JDK 25 sources)
// identity-type: (P)P
// identity-param-is-P: true
// identity-return-is-P: true
// exact-erased: java.lang.invoke.WrongMethodTypeException: handle's method type (P)P but found (Object)Object
// invoke-right: true
// invoke-wrong: java.lang.ClassCastException: Cannot cast java.lang.String to P
// asType-return-is-P: true
// asType-wrong: java.lang.ClassCastException: Cannot cast java.lang.String to P
// bindTo-right: true
// methodType-param-is-P: true
// again-exact-erased: java.lang.invoke.WrongMethodTypeException: handle's method type (P)P but found (Object)Object
// again-invoke-wrong: java.lang.ClassCastException: Cannot cast java.lang.String to P
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.IntUnaryOperator;

public class L4W92UnnameableMirror {
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
        MethodHandle id = MethodHandles.identity(p);
        row("identity-type", () -> id.type());
        row("identity-param-is-P", () -> id.type().parameterType(0) == p);
        row("identity-return-is-P", () -> id.type().returnType() == p);
        row("exact-erased", () -> {
            Object o = (Object) id.invokeExact((Object) f);
            return "ran";
        });
        row("invoke-right", () -> id.invoke((Object) f) == f);
        row("invoke-wrong", () -> {
            Object o = id.invoke((Object) "x");
            return "ran";
        });
        MethodHandle toP = MethodHandles.identity(Object.class)
                .asType(MethodType.methodType(p, Object.class));
        row("asType-return-is-P", () -> toP.type().returnType() == p);
        row("asType-wrong", () -> {
            Object o = toP.invoke((Object) "x");
            return "ran";
        });
        row("bindTo-right", () -> id.bindTo(f).invoke() == f);
        row("methodType-param-is-P",
                () -> MethodType.methodType(void.class, p).parameterType(0) == p);
        for (int i = 0; i < 2000; i++) {
            if (id.invoke((Object) f) != f) {
                throw new AssertionError();
            }
        }
        row("again-exact-erased", () -> {
            Object o = (Object) id.invokeExact((Object) f);
            return "ran";
        });
        row("again-invoke-wrong", () -> {
            Object o = id.invoke((Object) "x");
            return "ran";
        });
    }
}
