// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 52, lane L3 (review) -- answers of the served
// `Class` member lookups that differed from HotSpot 25:
//
//   * a static interface method is not a member of an implementing class
//     (JLS 8.4.8; JDK 25's `getMethodsRecursive` passes `includeStatic=false`
//     to every superinterface), so `ArrayList.class.getMethod("of")` is a
//     `NoSuchMethodException` while `List.class.getMethod("of")` finds it; the
//     Bridge found `List.of` through `ArrayList`;
//   * a primitive's miss names the primitive (`methodToString`: `int.x()`);
//     the Bridge's message was the bare name `x`;
//   * `ClassCastException` from `Class.cast` / `asSubclass` starts at the JDK
//     method (`--compatible` serves both with Bridges).
//
// Each row prints the answer, or the throwable's class, message and the JDK
// frames above the probe's first frame.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   static-via-impl, static-on-iface, static-via-subiface, static-via-super,
//   default-via-impl, prim-miss, prim-array-miss, cast, as-subclass,
//   methods-of-impl, static-via-impl-again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// static-via-impl: java.lang.NoSuchMethodException msg=java.util.ArrayList.of() [java.lang.Class.getMethod]
// static-on-iface: public static java.util.List java.util.List.of()
// static-via-subiface: java.lang.NoSuchMethodException msg=L3W52ClassLookupAnswers$Sub.make() [java.lang.Class.getMethod]
// static-via-super: public static int L3W52ClassLookupAnswers$Base.twice(int)
// default-via-impl: public default void L3W52ClassLookupAnswers$Iface.hello()
// prim-miss: java.lang.NoSuchMethodException msg=int.x() [java.lang.Class.getMethod]
// prim-array-miss: java.lang.NoSuchMethodException msg=[I.x(int) [java.lang.Class.getMethod]
// cast: java.lang.ClassCastException msg=Cannot cast java.lang.Integer to java.lang.String [java.lang.Class.cast]
// as-subclass: java.lang.ClassCastException msg=class L3W52ClassLookupAnswers [java.lang.Class.asSubclass]
// methods-of-impl: make=false hello=true twice=true
// static-via-impl-again: java.lang.NoSuchMethodException msg=java.util.ArrayList.of() [java.lang.Class.getMethod]
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W52ClassLookupAnswers`.
import java.util.ArrayList;
import java.util.List;

public class L3W52ClassLookupAnswers {
    public interface Iface {
        static Iface make() {
            return null;
        }

        default void hello() {
        }
    }

    public interface Sub extends Iface {
    }

    public static class Base {
        public static int twice(int x) {
            return 2 * x;
        }
    }

    public static class Impl extends Base implements Sub {
    }

    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            out = String.valueOf(call.run());
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            for (int i = 0; i < st.length && !st[i].getClassName().startsWith("L3W52"); i++) {
                jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
            }
            out = t.getClass().getName() + " msg=" + t.getMessage() + " [" + jdk + "]";
        }
        System.out.println(name + ": " + out);
    }

    static boolean has(Class<?> c, String name) {
        for (var m : c.getMethods()) {
            if (m.getName().equals(name)) {
                return true;
            }
        }
        return false;
    }

    public static void main(String[] args) {
        row("static-via-impl", () -> ArrayList.class.getMethod("of"));
        row("static-on-iface", () -> List.class.getMethod("of"));
        row("static-via-subiface", () -> Sub.class.getMethod("make"));
        row("static-via-super", () -> Impl.class.getMethod("twice", int.class));
        row("default-via-impl", () -> Impl.class.getMethod("hello"));
        row("prim-miss", () -> int.class.getMethod("x"));
        row("prim-array-miss", () -> int[].class.getMethod("x", int.class));
        row("cast", () -> String.class.cast(1));
        row("as-subclass", () -> L3W52ClassLookupAnswers.class.asSubclass(String.class));
        row("methods-of-impl", () -> "make=" + has(Impl.class, "make") + " hello=" + has(Impl.class, "hello")
                + " twice=" + has(Impl.class, "twice"));
        row("static-via-impl-again", () -> ArrayList.class.getMethod("of"));
    }
}
