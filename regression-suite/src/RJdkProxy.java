import java.lang.reflect.InvocationHandler;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.lang.reflect.Proxy;
import java.lang.reflect.UndeclaredThrowableException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/**
 * JDK-only corpus: dynamic proxies -- proxy byte generation, invocation
 * handlers, loader identity.
 *
 * A proxy class is legitimately VM-generated ({@code ClassOrigin::GeneratedProxy}
 * per the contract, NOT a compatibility stub), so this vector must keep passing
 * under {@code --jdk-only}.
 *
 * Determinism: the generated proxy class NAME ({@code com.sun.proxy.$Proxy17},
 * or {@code jdk.proxy1.$Proxy0} on newer images) is counter-derived and is never
 * printed -- only shape predicates are.
 */
public class RJdkProxy {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    public interface Service {
        String greet(String name);

        int add(int a, int b);

        default String twice(String s) {
            return greet(s) + greet(s);
        }

        void boom() throws java.io.IOException;
    }

    public interface Marker {
        String tag();
    }

    /** Records the calls it sees, so ordering can be asserted deterministically. */
    static final class Recorder implements InvocationHandler {
        final List<String> calls = new ArrayList<>();

        @Override
        public Object invoke(Object proxy, Method method, Object[] args) throws Throwable {
            calls.add(method.getName() + "/" + (args == null ? 0 : args.length));
            switch (method.getName()) {
                case "greet":
                    return "hi " + args[0];
                case "add":
                    return ((Integer) args[0]) + ((Integer) args[1]);
                case "tag":
                    return "T";
                case "twice":
                    // Java 16+: run the interface's real default body, which
                    // re-enters this handler through the proxy for each greet().
                    return InvocationHandler.invokeDefault(proxy, method, args);
                case "boom":
                    throw new java.io.IOException("declared");
                case "hashCode":
                    return 0x5EED;
                case "equals":
                    return proxy == args[0];
                case "toString":
                    return "RecorderProxy";
                default:
                    throw new UnsupportedOperationException(method.getName());
            }
        }
    }

    static void basics() throws Exception {
        Recorder r = new Recorder();
        ClassLoader loader = RJdkProxy.class.getClassLoader();
        Service s = (Service) Proxy.newProxyInstance(loader, new Class<?>[] { Service.class }, r);

        check(s.greet("bob").equals("hi bob"), "proxy method dispatch");
        check(s.add(20, 22) == 42, "proxy primitive boxing/unboxing");
        // A default method is NOT inherited by a proxy -- it is routed to the
        // handler like every other interface method.
        check(s.twice("x").equals("hi xhi x"), "default method routed through the handler");

        check(Proxy.isProxyClass(s.getClass()), "isProxyClass");
        check(!Proxy.isProxyClass(String.class), "isProxyClass(String) must be false");
        check(Proxy.getInvocationHandler(s) == r, "getInvocationHandler identity");
        check(s instanceof Service, "proxy implements its interface");
        check(s.getClass().getInterfaces().length == 1, "proxy interface count");
        check(s.getClass().getInterfaces()[0] == Service.class, "proxy interface identity");
        check(Modifier.isFinal(s.getClass().getModifiers()), "proxy classes are final");
        check(s.getClass().getSuperclass() == Proxy.class, "proxy superclass is java.lang.reflect.Proxy");
        check(!s.getClass().isHidden(), "proxy classes are ordinary (not hidden) classes");
        check(!s.getClass().isInterface(), "proxy class is a class, not an interface");

        // Object methods: hashCode/equals/toString are routed to the handler.
        check(s.hashCode() == 0x5EED, "Object.hashCode routed to handler");
        check(s.equals(s), "Object.equals routed to handler");
        check(!s.equals("other"), "Object.equals(other)");
        check(s.toString().equals("RecorderProxy"), "Object.toString routed to handler");
        // getClass() is final on Object and is NOT routed. `!= null` could not
        // fail -- getClass never returns null on any VM -- so it asserted the
        // comment above it and nothing else. The routed/not-routed question has
        // an observable: the handler records every call it receives.
        int callsBefore = r.calls.size();
        Class<?> live = s.getClass();
        check(live != null, "getClass returned null");
        check(r.calls.size() == callsBefore,
                "getClass must NOT be routed to the InvocationHandler, but it recorded "
                        + r.calls.subList(callsBefore, r.calls.size()));

        check(r.calls.equals(Arrays.asList(
                "greet/1", "add/2", "twice/1", "greet/1", "greet/1",
                "hashCode/0", "equals/1", "equals/1", "toString/0")),
                "recorded call sequence: " + r.calls);
        System.out.println("CK RJdkProxy calls=" + r.calls);
    }

    static void loaderIdentityAndCaching() {
        Recorder r = new Recorder();
        ClassLoader loader = RJdkProxy.class.getClassLoader();
        Class<?> a = Proxy.getProxyClass(loader, Service.class);
        Class<?> b = Proxy.getProxyClass(loader, Service.class);
        check(a == b, "proxy classes must be cached per (loader, interface list)");
        Class<?> two = Proxy.getProxyClass(loader, Service.class, Marker.class);
        check(two != a, "a different interface list yields a different proxy class");
        check(Proxy.getProxyClass(loader, Marker.class, Service.class) != two,
                "interface ORDER is part of the proxy class key");

        // The proxy class is defined to the loader we asked for (or a child of
        // it for non-public interfaces; ours are public, so it is exactly it).
        check(a.getClassLoader() == loader, "proxy loader identity");
        Object p = Proxy.newProxyInstance(loader, new Class<?>[] { Service.class, Marker.class }, r);
        check(p instanceof Service && p instanceof Marker, "multi-interface proxy");
        check(((Marker) p).tag().equals("T"), "second interface dispatch");
        check(p.getClass() == two, "instance uses the cached class");

        List<String> ifaces = new ArrayList<>();
        for (Class<?> i : two.getInterfaces()) {
            ifaces.add(i.getSimpleName());
        }
        Collections.sort(ifaces);
        check(ifaces.equals(Arrays.asList("Marker", "Service")), "interfaces: " + ifaces);
        System.out.println("CK RJdkProxy ifaces=" + ifaces + " cached=" + (a == b));
    }

    static void exceptionSemantics() throws Exception {
        ClassLoader loader = RJdkProxy.class.getClassLoader();

        // A declared checked exception passes through unwrapped.
        Service declared = (Service) Proxy.newProxyInstance(loader,
                new Class<?>[] { Service.class }, new Recorder());
        boolean threw = false;
        try {
            declared.boom();
        } catch (java.io.IOException e) {
            threw = "declared".equals(e.getMessage());
        }
        check(threw, "a declared checked exception must pass through the proxy unwrapped");

        // An UNdeclared checked exception is wrapped in UndeclaredThrowableException.
        Service undeclared = (Service) Proxy.newProxyInstance(loader,
                new Class<?>[] { Service.class },
                (proxy, method, args) -> {
                    throw new java.text.ParseException("undeclared", 0);
                });
        threw = false;
        try {
            undeclared.greet("x");
        } catch (UndeclaredThrowableException e) {
            threw = e.getCause() instanceof java.text.ParseException
                    && "undeclared".equals(e.getCause().getMessage());
        }
        check(threw, "an undeclared checked exception must be wrapped");

        // A RuntimeException is never wrapped.
        Service runtime = (Service) Proxy.newProxyInstance(loader,
                new Class<?>[] { Service.class },
                (proxy, method, args) -> {
                    throw new IllegalStateException("rt");
                });
        threw = false;
        try {
            runtime.greet("x");
        } catch (IllegalStateException e) {
            threw = "rt".equals(e.getMessage());
        }
        check(threw, "a RuntimeException must not be wrapped");

        // Returning null for a primitive-returning method is a NullPointerException.
        Service badNull = (Service) Proxy.newProxyInstance(loader,
                new Class<?>[] { Service.class }, (proxy, method, args) -> null);
        threw = false;
        try {
            badNull.add(1, 2);
        } catch (NullPointerException expected) {
            threw = true;
        }
        check(threw, "null for an int-returning proxy method must NPE");

        // A wrong return type is a ClassCastException.
        Service badType = (Service) Proxy.newProxyInstance(loader,
                new Class<?>[] { Service.class }, (proxy, method, args) -> Integer.valueOf(1));
        threw = false;
        try {
            badType.greet("x");
        } catch (ClassCastException expected) {
            threw = true;
        }
        check(threw, "a wrong proxy return type must throw ClassCastException");

        // A non-interface argument is rejected.
        threw = false;
        try {
            Proxy.newProxyInstance(loader, new Class<?>[] { String.class }, new Recorder());
        } catch (IllegalArgumentException expected) {
            threw = true;
        }
        check(threw, "Proxy over a non-interface must be rejected");
        System.out.println("CK RJdkProxy exceptions ok");
    }

    static void reflectionOverProxy() throws Exception {
        ClassLoader loader = RJdkProxy.class.getClassLoader();
        Recorder r = new Recorder();
        Object p = Proxy.newProxyInstance(loader, new Class<?>[] { Service.class }, r);
        Method greet = Service.class.getMethod("greet", String.class);
        check("hi z".equals(greet.invoke(p, "z")), "reflective invoke on a proxy");

        Method boom = Service.class.getMethod("boom");
        boolean threw = false;
        try {
            boom.invoke(p);
        } catch (InvocationTargetException e) {
            threw = e.getCause() instanceof java.io.IOException;
        }
        check(threw, "reflective invoke wraps the proxy's exception in InvocationTargetException");
        check(r.calls.size() == 2, "handler saw both reflective calls: " + r.calls);
        System.out.println("CK RJdkProxy reflect=" + r.calls);
    }

    public static void main(String[] args) throws Exception {
        basics();
        loaderIdentityAndCaching();
        exceptionSemantics();
        reflectionOverProxy();
        System.out.println("CK RJdkProxy checks=" + checks);
        System.out.println("PASS RJdkProxy (" + checks + " checks)");
    }
}
