// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 61, lane L4 (review of the lambda-proxy member
// handles, `L4W61LambdaProxyMemberHandles`): a FIELD handle over a lambda
// proxy's captured `arg$N` field. A proxy's mirror names no class, so the
// finders minted the getter over the class `""`, typed `(L;)T` with a
// stand-in mirror: `type().toString()` raised `NullPointerException`
// (`--jdk-only`) or printed `(?)String` (`--compatible`), and an absent
// field or a setter of the final capture got a handle instead of HotSpot's
// refusal. The proxy's hidden-class name is masked as `P`; a refusal's
// `, from <lookup class>` tail (it names a module hash) is cut.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// findGetter-arg1: (P)String cap
// findGetter-arg2: (P)int 42
// unreflectGetter: (P)String cap
// findGetter-wrong-type: java.lang.NoSuchFieldException: no such field: P.arg$2/long/getField
// findGetter-absent: java.lang.NoSuchFieldException: no such field: P.arg$9/int/getField
// findSetter-final: java.lang.IllegalAccessException: unexpected set of a final field: P.arg$2/int/putField
// unreflectSetter-accessible: java.lang.IllegalAccessException: final field has no write access: P.arg$2/int/putField
// findGetter-arg1-again: (P)String cap
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W61LambdaProxyFieldHandles`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Field;
import java.util.function.Supplier;

public class L4W61LambdaProxyFieldHandles {
    interface Row {
        Object run() throws Throwable;
    }

    static String proxyName = "\0";

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
            int from = out.indexOf(", from ");
            if (from >= 0) {
                out = out.substring(0, from);
            }
        }
        System.out.println(name + ": " + out.replace(proxyName, "P"));
    }

    static Supplier<String> make(String cap, int n) {
        return () -> cap + n;
    }

    public static void main(String[] args) throws Throwable {
        Supplier<String> s = make("cap", 42);
        Class<?> proxy = s.getClass();
        proxyName = proxy.getName();
        MethodHandles.Lookup l = MethodHandles.privateLookupIn(proxy, MethodHandles.lookup());
        row("findGetter-arg1", () -> {
            MethodHandle g = l.findGetter(proxy, "arg$1", String.class);
            return g.type() + " " + g.invoke(s);
        });
        row("findGetter-arg2", () -> {
            MethodHandle g = l.findGetter(proxy, "arg$2", int.class);
            return g.type() + " " + g.invoke(s);
        });
        row("unreflectGetter", () -> {
            Field f = proxy.getDeclaredField("arg$1");
            MethodHandle g = l.unreflectGetter(f);
            return g.type() + " " + g.invoke(s);
        });
        row("findGetter-wrong-type", () -> l.findGetter(proxy, "arg$2", long.class));
        row("findGetter-absent", () -> l.findGetter(proxy, "arg$9", int.class));
        row("findSetter-final", () -> l.findSetter(proxy, "arg$2", int.class));
        row("unreflectSetter-accessible", () -> {
            Field f = proxy.getDeclaredField("arg$2");
            f.setAccessible(true);
            return l.unreflectSetter(f);
        });
        row("findGetter-arg1-again", () -> {
            MethodHandle g = l.findGetter(proxy, "arg$1", String.class);
            return g.type() + " " + g.invoke(s);
        });
    }
}
