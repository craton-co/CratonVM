// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 85, lane L4: `MethodHandleProxies` against JDK
// 25's own implementation (a hidden class per interface, spun with the
// ClassFile API into a dynamic module `jdk.MHProxyN`, registered in a
// `WRAPPER_TYPES` set; `isWrapperInstance(x)` is
// `x != null && WRAPPER_TYPES.contains(x.getClass())`;
// `wrapperInstanceTarget` reads the instance's target field and
// `wrapperInstanceType` answers the INTERFACE (a `Class`); both refuse a
// non-wrapper with `IllegalArgumentException("not a wrapper instance: " + x)`;
// `asInterfaceInstance` refuses a non-public / non-interface class
// ("not a public interface: N"), a sealed one ("a sealed interface: N") and a
// non-SAM one ("not a single-method interface: N"); the generated method
// rethrows a declared or unchecked throwable and wraps any other in
// `UndeclaredThrowableException`; a target that cannot `asType` to the SAM's
// type fails in the proxy's constructor with the `WrongMethodTypeException`
// (`uncaughtException` rethrows a RuntimeException as is)). The registered
// stand-ins of `register_p68_invoke_extras` are dropped against a real image,
// so this is the JDK's code in both modes.
//
// Run: javac -d . L4W85MethodHandleProxies.java
//      cratonvm --jdk-only [--nojit] -cp . L4W85MethodHandleProxies
//      cratonvm --compatible [--nojit] -cp . L4W85MethodHandleProxies
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// call: hi bob
// class: hidden=true name-shape=true module-named=true module-prefix=true
// shared-class: true distinct=true
// isWrapperInstance: proxy=true handle=false string=false null=false lambda=false
// wrapperInstanceTarget: same=true
// wrapperInstanceType: Greeter
// target-of-non-wrapper: java.lang.IllegalArgumentException: not a wrapper instance: x
// type-of-non-wrapper: java.lang.IllegalArgumentException: not a wrapper instance: x
// not-an-interface: java.lang.IllegalArgumentException: not a public interface: java.lang.String
// package-private: java.lang.IllegalArgumentException: not a public interface: L4W85MethodHandleProxies$Priv
// sealed: java.lang.IllegalArgumentException: a sealed interface: L4W85MethodHandleProxies$Sealed
// two-methods: java.lang.IllegalArgumentException: not a single-method interface: L4W85MethodHandleProxies$Two
// default-method: default:hi ann
// declared-checked: java.io.IOException: io
// undeclared-checked: java.lang.reflect.UndeclaredThrowableException <- java.io.IOException: io
// unchecked: java.lang.IllegalStateException: ise
// wrong-target-type: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()int to (String)String
// HOTSPOT_EXPECTED_END
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleProxies;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W85MethodHandleProxies {
    public interface Greeter {
        String greet(String who);

        default String twice(String who) {
            return "default:" + greet(who);
        }
    }

    interface Priv {
        void run();
    }

    public sealed interface Sealed permits SealedImpl {
        void run();
    }

    public static final class SealedImpl implements Sealed {
        public void run() {
        }
    }

    public interface Two {
        void a();

        void b();
    }

    public interface IoRunner {
        void run() throws IOException;
    }

    public static String hi(String who) {
        return "hi " + who;
    }

    public static int answer() {
        return 42;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static String chain(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (Throwable c = t; c != null; c = c.getCause()) {
            if (sb.length() > 0) {
                sb.append(" <- ");
            }
            sb.append(c.getClass().getName());
            if (c.getMessage() != null) {
                sb.append(": ").append(c.getMessage());
            }
        }
        return sb.toString();
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = chain(t);
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle hi = l.findStatic(L4W85MethodHandleProxies.class, "hi",
                MethodType.methodType(String.class, String.class));
        Greeter g = MethodHandleProxies.asInterfaceInstance(Greeter.class, hi);
        Greeter g2 = MethodHandleProxies.asInterfaceInstance(Greeter.class, hi);
        Runnable lambda = () -> { };

        row("call", () -> g.greet("bob"));
        row("class", () -> {
            Class<?> c = g.getClass();
            return "hidden=" + c.isHidden()
                    + " name-shape=" + c.getName().matches(
                            "jdk\\.MHProxy\\d+\\.L4W85MethodHandleProxies\\$Greeter/0x[0-9a-f]+")
                    + " module-named=" + c.getModule().isNamed()
                    + " module-prefix=" + String.valueOf(c.getModule().getName()).startsWith("jdk.MHProxy");
        });
        row("shared-class", () -> (g.getClass() == g2.getClass()) + " distinct=" + (g != g2));
        row("isWrapperInstance", () -> "proxy=" + MethodHandleProxies.isWrapperInstance(g)
                + " handle=" + MethodHandleProxies.isWrapperInstance(hi)
                + " string=" + MethodHandleProxies.isWrapperInstance("x")
                + " null=" + MethodHandleProxies.isWrapperInstance(null)
                + " lambda=" + MethodHandleProxies.isWrapperInstance(lambda));
        row("wrapperInstanceTarget", () -> "same=" + (MethodHandleProxies.wrapperInstanceTarget(g) == hi));
        row("wrapperInstanceType", () -> MethodHandleProxies.wrapperInstanceType(g).getSimpleName());
        row("target-of-non-wrapper", () -> MethodHandleProxies.wrapperInstanceTarget("x"));
        row("type-of-non-wrapper", () -> MethodHandleProxies.wrapperInstanceType("x"));
        row("not-an-interface", () -> MethodHandleProxies.asInterfaceInstance(String.class, hi));
        row("package-private", () -> MethodHandleProxies.asInterfaceInstance(Priv.class, hi));
        row("sealed", () -> MethodHandleProxies.asInterfaceInstance(Sealed.class, hi));
        row("two-methods", () -> MethodHandleProxies.asInterfaceInstance(Two.class, hi));
        row("default-method", () -> g.twice("ann"));

        MethodHandle throwIo = MethodHandles.throwException(void.class, IOException.class)
                .bindTo(new IOException("io"));
        row("declared-checked", () -> {
            MethodHandleProxies.asInterfaceInstance(IoRunner.class, throwIo).run();
            return "no exception";
        });
        row("undeclared-checked", () -> {
            MethodHandleProxies.asInterfaceInstance(Runnable.class, throwIo).run();
            return "no exception";
        });
        MethodHandle throwIse = MethodHandles.throwException(void.class, IllegalStateException.class)
                .bindTo(new IllegalStateException("ise"));
        row("unchecked", () -> {
            MethodHandleProxies.asInterfaceInstance(Runnable.class, throwIse).run();
            return "no exception";
        });
        MethodHandle answer = l.findStatic(L4W85MethodHandleProxies.class, "answer", MethodType.methodType(int.class));
        row("wrong-target-type", () -> MethodHandleProxies.asInterfaceInstance(Greeter.class, answer));
    }
}
