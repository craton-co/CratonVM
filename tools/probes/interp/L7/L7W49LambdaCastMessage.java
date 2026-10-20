// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L7: a failing `checkcast` of a LAMBDA
// printed `?class_id=2147483658 cannot be cast to java.lang.String` (a lambda
// proxy has no class in the class store, and neither the opcode nor the
// message funnel knew its name or its module and loader). HotSpot names the
// hidden lambda class as `getClass().getName()` does, and its host's module
// and loader:
//
//   class P$$Lambda/0x... cannot be cast to class java.lang.String
//   (P$$Lambda/0x... is in unnamed module of loader 'app'; java.lang.String is
//   in module java.base of loader 'bootstrap')
//
// The hidden class's address differs per run and per VM, so every
// `/0x<hex>` is printed as `/0x?`; the `name-matches` rows check that the
// message names exactly `getClass().getName()`. Rows run cold and again warm.
//
//   cast-string      (String) lambda             two clauses
//   cast-iface       (Function) supplier lambda  two clauses
//   cast-app         (P.Marker) lambda           one joint clause (same module
//                                                and loader)
//   cast-methodref   (String) String::length     a method-reference proxy
//   instanceof       lambda instanceof String (no exception)
//
// Expected (HotSpot 25.0.3, `java L7W49LambdaCastMessage`, and `-Xint`):
//   r0 cast-string ClassCastException: class L7W49LambdaCastMessage$$Lambda/0x? cannot be cast to class java.lang.String (L7W49LambdaCastMessage$$Lambda/0x? is in unnamed module of loader 'app'; java.lang.String is in module java.base of loader 'bootstrap')
//   r0 cast-string name-matches true
//   r0 cast-iface ClassCastException: class L7W49LambdaCastMessage$$Lambda/0x? cannot be cast to class java.util.function.Function (L7W49LambdaCastMessage$$Lambda/0x? is in unnamed module of loader 'app'; java.util.function.Function is in module java.base of loader 'bootstrap')
//   r0 cast-iface name-matches true
//   r0 cast-app ClassCastException: class L7W49LambdaCastMessage$$Lambda/0x? cannot be cast to class L7W49LambdaCastMessage$Marker (L7W49LambdaCastMessage$$Lambda/0x? and L7W49LambdaCastMessage$Marker are in unnamed module of loader 'app')
//   r0 cast-app name-matches true
//   r0 cast-methodref ClassCastException: class L7W49LambdaCastMessage$$Lambda/0x? cannot be cast to class java.lang.String (L7W49LambdaCastMessage$$Lambda/0x? is in unnamed module of loader 'app'; java.lang.String is in module java.base of loader 'bootstrap')
//   r0 cast-methodref name-matches true
//   r0 instanceof false
//   r1 cast-string ClassCastException: class L7W49LambdaCastMessage$$Lambda/0x? cannot be cast to class java.lang.String (L7W49LambdaCastMessage$$Lambda/0x? is in unnamed module of loader 'app'; java.lang.String is in module java.base of loader 'bootstrap')
//   r1 cast-string name-matches true
//   r1 cast-iface ClassCastException: class L7W49LambdaCastMessage$$Lambda/0x? cannot be cast to class java.util.function.Function (L7W49LambdaCastMessage$$Lambda/0x? is in unnamed module of loader 'app'; java.util.function.Function is in module java.base of loader 'bootstrap')
//   r1 cast-iface name-matches true
//   r1 cast-app ClassCastException: class L7W49LambdaCastMessage$$Lambda/0x? cannot be cast to class L7W49LambdaCastMessage$Marker (L7W49LambdaCastMessage$$Lambda/0x? and L7W49LambdaCastMessage$Marker are in unnamed module of loader 'app')
//   r1 cast-app name-matches true
//   r1 cast-methodref ClassCastException: class L7W49LambdaCastMessage$$Lambda/0x? cannot be cast to class java.lang.String (L7W49LambdaCastMessage$$Lambda/0x? is in unnamed module of loader 'app'; java.lang.String is in module java.base of loader 'bootstrap')
//   r1 cast-methodref name-matches true
//   r1 instanceof false
import java.util.function.Function;
import java.util.function.Supplier;

public class L7W49LambdaCastMessage {
    interface Marker {
    }

    static Object id(Object o) {
        return o;
    }

    static String norm(String s) {
        return s.replaceAll("/0x[0-9a-fA-F]+", "/0x?");
    }

    interface Body {
        void run() throws Throwable;
    }

    static void row(String label, Object subject, Body body) {
        try {
            body.run();
            System.out.println(label + " no exception");
        } catch (Throwable t) {
            System.out.println(label + " " + t.getClass().getSimpleName() + ": " + norm(t.getMessage()));
            String name = subject.getClass().getName();
            System.out.println(label + " name-matches " + t.getMessage().startsWith("class " + name + " cannot be cast"));
        }
    }

    public static void main(String[] args) {
        for (int r = 0; r < 2; r++) {
            String p = "r" + r + " ";
            Object run = id((Runnable) () -> { });
            row(p + "cast-string", run, () -> {
                String s = (String) run;
            });
            Object sup = id((Supplier<String>) () -> "x");
            row(p + "cast-iface", sup, () -> {
                Function<?, ?> f = (Function<?, ?>) sup;
            });
            Object any = id((Runnable) () -> { });
            row(p + "cast-app", any, () -> {
                Marker m = (Marker) any;
            });
            Object ref = id((Function<String, Integer>) String::length);
            row(p + "cast-methodref", ref, () -> {
                String s = (String) ref;
            });
            System.out.println(p + "instanceof " + (id(run) instanceof String));
        }
    }
}
