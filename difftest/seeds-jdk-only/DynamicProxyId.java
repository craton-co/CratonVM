// difftest: strict
//
// JDK-only boundary vector: java.lang.reflect.Proxy
// (docs/feature-designs/jdk-only-mode.md §1.6 — a dynamic proxy is *allowed*
// under `--jdk-only` and carries origin `generated-proxy`, so it must NOT be
// refused as a fabricated class, and must NOT be quietly served by a
// compatibility stub either).
//
// The vector prints the two things a fabricated stand-in gets wrong and a real
// generated proxy gets right: **assignability** (the proxy class really
// implements the requested interfaces, extends java.lang.reflect.Proxy, and is
// recognised by Proxy.isProxyClass) and **loader identity** (it is defined to
// the loader that was passed in). Generated class *names* are never printed —
// `$Proxy0` / `jdk.proxy1.$Proxy0` is an implementation detail.
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.lang.reflect.UndeclaredThrowableException;

public class DynamicProxyId {

    public interface Greeter {
        String greet(String who);

        int count();
    }

    public interface Marker {
    }

    /** Throws a checked exception the proxy interface does not declare. */
    public interface Strict {
        void run() throws java.io.IOException;
    }

    public static void main(String[] args) {
        ClassLoader app = DynamicProxyId.class.getClassLoader();

        InvocationHandler handler = new InvocationHandler() {
            @Override
            public Object invoke(Object proxy, Method method, Object[] a) {
                switch (method.getName()) {
                    case "greet":
                        return "hi " + a[0];
                    case "count":
                        return 7;
                    case "toString":
                        return "proxy-toString";
                    case "hashCode":
                        return 1234;
                    case "equals":
                        return proxy == a[0];
                    default:
                        return null;
                }
            }
        };

        Object raw = Proxy.newProxyInstance(app, new Class<?>[] { Greeter.class, Marker.class },
                handler);
        Greeter g = (Greeter) raw;
        Class<?> pc = raw.getClass();

        // --- behaviour routed through the handler --------------------------
        System.out.println("greet: " + g.greet("bob"));
        System.out.println("count: " + g.count());
        System.out.println("toString: " + g.toString());
        System.out.println("hashCode: " + g.hashCode());
        System.out.println("equals-self: " + g.equals(g));
        System.out.println("equals-other: " + g.equals("nope"));

        // --- generated-class identity: assignability -----------------------
        System.out.println("is-proxy-class: " + Proxy.isProxyClass(pc));
        System.out.println("is-proxy-instance: " + Proxy.isProxyClass(raw.getClass()));
        System.out.println("assignable-Greeter: " + Greeter.class.isAssignableFrom(pc));
        System.out.println("assignable-Marker: " + Marker.class.isAssignableFrom(pc));
        System.out.println("instanceof-Greeter: " + (raw instanceof Greeter));
        System.out.println("instanceof-Marker: " + (raw instanceof Marker));
        System.out.println("instanceof-Strict: " + (raw instanceof Strict));
        System.out.println("super: " + pc.getSuperclass().getName());
        System.out.println("iface-count: " + pc.getInterfaces().length);
        System.out.println("iface0: " + pc.getInterfaces()[0].getName());
        System.out.println("iface1: " + pc.getInterfaces()[1].getName());
        System.out.println("is-iface: " + pc.isInterface());
        System.out.println("is-array: " + pc.isArray());

        // --- generated-class identity: loader ------------------------------
        System.out.println("loader-is-app: " + (pc.getClassLoader() == app));
        System.out.println("handler-roundtrip: " + (Proxy.getInvocationHandler(raw) == handler));
        // A second proxy for the same (loader, interfaces) reuses the class.
        Object again = Proxy.newProxyInstance(app, new Class<?>[] { Greeter.class, Marker.class },
                handler);
        System.out.println("class-cached: " + (again.getClass() == pc));
        System.out.println("instance-distinct: " + (again != raw));
        // Interface order is part of the key, so a different order is a
        // different proxy class.
        Object swapped = Proxy.newProxyInstance(app, new Class<?>[] { Marker.class, Greeter.class },
                handler);
        System.out.println("order-matters: " + (swapped.getClass() != pc));

        // --- error surface -------------------------------------------------
        try {
            Proxy.newProxyInstance(app, new Class<?>[] { String.class }, handler);
            System.out.println("no-IAE");
        } catch (IllegalArgumentException e) {
            System.out.println("IAE-for-non-interface: true");
        }
        try {
            Proxy.getInvocationHandler("not a proxy");
            System.out.println("no-IAE-2");
        } catch (IllegalArgumentException e) {
            System.out.println("IAE-for-non-proxy: true");
        }
        // An undeclared checked exception from the handler must be wrapped.
        Strict s = (Strict) Proxy.newProxyInstance(app, new Class<?>[] { Strict.class },
                (proxy, method, a) -> {
                    throw new IllegalStateException("boom");
                });
        try {
            s.run();
            System.out.println("no-throw");
        } catch (IllegalStateException e) {
            // An *unchecked* exception passes through unwrapped.
            System.out.println("ISE-passthrough: " + e.getMessage());
        } catch (Throwable t) {
            System.out.println("unexpected: " + t.getClass().getName());
        }
        Strict checked = (Strict) Proxy.newProxyInstance(app, new Class<?>[] { Strict.class },
                (proxy, method, a) -> {
                    throw new InterruptedException("undeclared");
                });
        try {
            checked.run();
            System.out.println("no-throw-2");
        } catch (UndeclaredThrowableException e) {
            System.out.println("UTE-cause: " + e.getCause().getClass().getName());
        } catch (Throwable t) {
            System.out.println("unexpected-2: " + t.getClass().getName());
        }

        System.out.println("done");
    }
}
