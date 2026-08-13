// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.Set;
import java.util.TreeSet;

/**
 * WP2.5 six-case acceptance probe for {@code java.lang.reflect.Proxy}.
 *
 * <p>Driven by {@code vm/tests/wp2_5_proxy.rs}. The contract that file pins:
 *
 * <ul>
 *   <li>one {@code pass-N ...} line per passing case, one {@code fail-N ...}
 *       line per failing case, for N in 1..6;
 *   <li>a {@code summary <passed>/6} line;
 *   <li>a final line that is exactly {@code OK} (or {@code FAIL}).
 * </ul>
 *
 * <p><b>Every value printed on a pass line is read back out of the proxy.</b>
 * Nothing here prints a canned literal: each case computes a value by calling
 * through a real {@code Proxy.newProxyInstance} result and compares it to the
 * expectation, so a VM whose proxy support regresses turns the line into a
 * {@code fail-N} and drops the summary below {@code 6/6}.
 *
 * <p>Constraints imposed by the callers, do not break them:
 *
 * <ul>
 *   <li>{@code main} is also invoked IN-PROCESS by
 *       {@code proxy_lambda_handler_dispatches_single_iface} with a <b>null</b>
 *       {@code String[]} — the parameter must never be dereferenced;
 *   <li>for the same reason there is no {@code System.exit} anywhere: it would
 *       take the cargo test process down with it;
 *   <li>output is deterministic (no lambda class names, no hash codes, no
 *       timings on any printed line) and the run is bounded — six proxies, a
 *       dozen dispatches, no loops.
 * </ul>
 */
public class ProxyProbe {

    /** Case 1/3/5/6 single-argument interface. */
    public interface Greeter {
        String hello(String who);
    }

    /** Case 3/5 second interface — primitive return, exercises unboxing. */
    public interface Counter {
        int count();
    }

    /** Case 6: the default method the proxy must route through the handler. */
    public interface PrefixedGreeter extends Greeter {
        default String prefixed(String who) {
            return "default:" + who;
        }
    }

    /**
     * A plain (non-lambda) {@link InvocationHandler}. Cases 4 and 5 use this so
     * the non-lambda dispatch path is covered too, and so case 4 can print a
     * STABLE handler class name (a lambda's is {@code $$Lambda/0x...}).
     */
    static final class Recorder implements InvocationHandler {
        /** Method names in dispatch order, e.g. {@code "hello;count;"}. */
        final StringBuilder seen = new StringBuilder();
        /** Interface dispatches so far, including the one in flight. */
        int dispatches;

        @Override
        public Object invoke(Object proxy, Method m, Object[] args) {
            Object direct = objectMethod(proxy, m, args);
            if (direct != NOT_OBJECT_METHOD) {
                return direct;
            }
            dispatches++;
            seen.append(m.getName()).append(';');
            String name = m.getName();
            if (name.equals("hello")) {
                return "hi " + args[0];
            }
            if (name.equals("count")) {
                // Derived from the handler having actually run twice: this is
                // what makes case 5 prove BOTH interfaces reached the SAME
                // handler rather than just that a cast succeeded.
                return dispatches;
            }
            if (name.equals("prefixed")) {
                return "handled-" + args[0];
            }
            throw new IllegalStateException("unexpected method " + name);
        }
    }

    // -----------------------------------------------------------------------

    public static void main(String[] args) {
        // `args` is null when this is invoked in-process by the Rust test.
        int passed = 0;
        passed += case1SingleIface() ? 1 : 0;
        passed += case2IsProxyClass() ? 1 : 0;
        passed += case3GetInterfaces() ? 1 : 0;
        passed += case4GetInvocationHandler() ? 1 : 0;
        passed += case5MultiIface() ? 1 : 0;
        passed += case6DefaultMethod() ? 1 : 0;
        System.out.println("summary " + passed + "/6");
        System.out.println(passed == 6 ? "OK" : "FAIL");
    }

    /** Case 1: single-interface proxy, LAMBDA handler, value round-trips. */
    static boolean case1SingleIface() {
        try {
            InvocationHandler h = (proxy, m, a) -> {
                Object direct = objectMethod(proxy, m, a);
                if (direct != NOT_OBJECT_METHOD) {
                    return direct;
                }
                if (m.getName().equals("hello")) {
                    return "hi " + a[0];
                }
                throw new IllegalStateException("unexpected method " + m.getName());
            };
            Object p = Proxy.newProxyInstance(loader(), new Class<?>[] {Greeter.class}, h);
            Greeter g = (Greeter) p;
            String got = g.hello("world");
            if ("hi world".equals(got)) {
                System.out.println("pass-1 single-iface hello=" + got);
                return true;
            }
            System.out.println("fail-1 single-iface expected=hi world got=" + got);
            return false;
        } catch (Throwable t) {
            System.out.println("fail-1 single-iface threw=" + describe(t));
            return false;
        }
    }

    /** Case 2: {@code isProxyClass} true on the proxy, false on a real class. */
    static boolean case2IsProxyClass() {
        try {
            Object p = Proxy.newProxyInstance(
                    loader(), new Class<?>[] {Greeter.class}, new Recorder());
            boolean onProxy = Proxy.isProxyClass(p.getClass());
            boolean onString = Proxy.isProxyClass(String.class);
            boolean onIface = Proxy.isProxyClass(Greeter.class);
            if (onProxy && !onString && !onIface) {
                System.out.println("pass-2 isProxyClass proxy=" + onProxy
                        + " String=" + onString + " Greeter=" + onIface);
                return true;
            }
            System.out.println("fail-2 isProxyClass expected=true/false/false got="
                    + onProxy + "/" + onString + "/" + onIface);
            return false;
        } catch (Throwable t) {
            System.out.println("fail-2 isProxyClass threw=" + describe(t));
            return false;
        }
    }

    /** Case 3: {@code getClass().getInterfaces()} round-trips the request. */
    static boolean case3GetInterfaces() {
        try {
            Class<?>[] want = {Greeter.class, Counter.class};
            Object p = Proxy.newProxyInstance(loader(), want, new Recorder());
            Class<?>[] got = p.getClass().getInterfaces();
            // Presence, not order — the spec does not pin the order.
            Set<String> wantNames = names(want);
            Set<String> gotNames = names(got);
            if (wantNames.equals(gotNames)) {
                System.out.println("pass-3 getInterfaces=" + gotNames);
                return true;
            }
            System.out.println("fail-3 getInterfaces expected=" + wantNames + " got=" + gotNames);
            return false;
        } catch (Throwable t) {
            System.out.println("fail-3 getInterfaces threw=" + describe(t));
            return false;
        }
    }

    /** Case 4: {@code getInvocationHandler} returns the SAME instance. */
    static boolean case4GetInvocationHandler() {
        try {
            Recorder h = new Recorder();
            Object p = Proxy.newProxyInstance(loader(), new Class<?>[] {Greeter.class}, h);
            InvocationHandler got = Proxy.getInvocationHandler(p);
            boolean same = (got == h);
            if (same) {
                System.out.println("pass-4 getInvocationHandler same=" + same
                        + " class=" + got.getClass().getName());
                return true;
            }
            System.out.println("fail-4 getInvocationHandler expected=same got="
                    + (got == null ? "null" : got.getClass().getName()));
            return false;
        } catch (Throwable t) {
            System.out.println("fail-4 getInvocationHandler threw=" + describe(t));
            return false;
        }
    }

    /** Case 5: a two-interface proxy dispatches BOTH through one handler. */
    static boolean case5MultiIface() {
        try {
            Recorder h = new Recorder();
            Object p = Proxy.newProxyInstance(
                    loader(), new Class<?>[] {Greeter.class, Counter.class}, h);
            Greeter g = (Greeter) p;
            Counter c = (Counter) p;
            String hello = g.hello("multi");
            int n = c.count();
            if ("hi multi".equals(hello) && n == 2) {
                System.out.println("pass-5 multi-iface hello=" + hello
                        + " count=" + n + " seen=" + h.seen);
                return true;
            }
            System.out.println("fail-5 multi-iface expected=hi multi/2 got="
                    + hello + "/" + n + " seen=" + h.seen);
            return false;
        } catch (Throwable t) {
            System.out.println("fail-5 multi-iface threw=" + describe(t));
            return false;
        }
    }

    /**
     * Case 6: an interface DEFAULT method must reach the handler, which
     * answers {@code handled-b}. If the proxy inherits the interface default
     * instead of overriding it the value is {@code default:b}, which is the
     * failure this case exists to name.
     */
    static boolean case6DefaultMethod() {
        try {
            InvocationHandler h = (proxy, m, a) -> {
                Object direct = objectMethod(proxy, m, a);
                if (direct != NOT_OBJECT_METHOD) {
                    return direct;
                }
                if (m.getName().equals("prefixed")) {
                    return "handled-" + a[0];
                }
                if (m.getName().equals("hello")) {
                    return "hi " + a[0];
                }
                throw new IllegalStateException("unexpected method " + m.getName());
            };
            Object p = Proxy.newProxyInstance(
                    loader(), new Class<?>[] {PrefixedGreeter.class}, h);
            PrefixedGreeter pg = (PrefixedGreeter) p;
            String prefixed = pg.prefixed("b");
            // The inherited super-interface method must be wired too.
            String hello = pg.hello("b");
            if ("handled-b".equals(prefixed) && "hi b".equals(hello)) {
                System.out.println("pass-6 default-method prefixed=" + prefixed
                        + " hello=" + hello);
                return true;
            }
            System.out.println("fail-6 default-method expected=handled-b/hi b got="
                    + prefixed + "/" + hello);
            return false;
        } catch (Throwable t) {
            System.out.println("fail-6 default-method threw=" + describe(t));
            return false;
        }
    }

    // -----------------------------------------------------------------------
    // helpers
    // -----------------------------------------------------------------------

    /** Sentinel: "this was not an Object method, keep going". */
    private static final Object NOT_OBJECT_METHOD = new Object();

    /**
     * Answer the three {@code java.lang.Object} methods a proxy routes through
     * the handler, so a stray {@code toString()} from the runtime cannot
     * recurse or throw. Keyed on name + arity rather than
     * {@code getDeclaringClass()} so this helper never becomes a second,
     * unasserted probe of the thing under test. None of the interfaces above
     * declare these names, so there is no collision.
     */
    static Object objectMethod(Object proxy, Method m, Object[] args) {
        int argc = (args == null) ? 0 : args.length;
        String name = m.getName();
        if (argc == 0 && name.equals("hashCode")) {
            return System.identityHashCode(proxy);
        }
        if (argc == 1 && name.equals("equals")) {
            return proxy == args[0];
        }
        if (argc == 0 && name.equals("toString")) {
            return "ProxyProbe-proxy";
        }
        return NOT_OBJECT_METHOD;
    }

    /** Sorted simple-name set, so the printed order is stable. */
    static Set<String> names(Class<?>[] cs) {
        Set<String> out = new TreeSet<String>();
        for (Class<?> c : cs) {
            out.add(c.getName());
        }
        return out;
    }

    static ClassLoader loader() {
        return ProxyProbe.class.getClassLoader();
    }

    static String describe(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }
}
