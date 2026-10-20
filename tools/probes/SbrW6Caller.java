// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;
import java.util.stream.Stream;

/** A class whose private members a non-nestmate reaches reflectively (SbrW6Caller). */
class SbrW6Target {
    private int f = 3;
    private static int sf = 4;

    private int secret() {
        return 7;
    }

    private static int ssecret() {
        return 8;
    }

    private SbrW6Target() {}

    static SbrW6Target make() {
        return new SbrW6Target();
    }
}

/** Not a nestmate of {@link SbrW6Target}: its reflective access is refused and the message names it. */
class SbrW6Outsider {
    static String viaMethod() {
        try {
            Method m = SbrW6Target.class.getDeclaredMethod("secret");
            return "ok" + m.invoke(SbrW6Target.make());
        } catch (Throwable t) {
            return SbrW6Caller.msg(t);
        }
    }

    static String viaMethodStatic() {
        try {
            Method m = SbrW6Target.class.getDeclaredMethod("ssecret");
            return "ok" + m.invoke(null);
        } catch (Throwable t) {
            return SbrW6Caller.msg(t);
        }
    }

    static String viaCtor() {
        try {
            Constructor<SbrW6Target> c = SbrW6Target.class.getDeclaredConstructor();
            return "ok" + (c.newInstance() != null);
        } catch (Throwable t) {
            return SbrW6Caller.msg(t);
        }
    }

    static String viaField() {
        try {
            Field f = SbrW6Target.class.getDeclaredField("f");
            return "ok" + f.getInt(SbrW6Target.make());
        } catch (Throwable t) {
            return SbrW6Caller.msg(t);
        }
    }

    static String viaStaticField() {
        try {
            Field f = SbrW6Target.class.getDeclaredField("sf");
            f.setInt(null, 5);
            return "ok";
        } catch (Throwable t) {
            return SbrW6Caller.msg(t);
        }
    }

    static String lookupClass() {
        return MethodHandles.lookup().lookupClass().getName();
    }

    static String walkerCaller(StackWalker sw) {
        try {
            return sw.getCallerClass().getName();
        } catch (Throwable t) {
            return SbrW6Caller.msg(t);
        }
    }

    /** {@code MethodHandles.lookup()} reached through {@code Method.invoke} from this class. */
    static String reflectiveLookup() throws Throwable {
        Method lk = MethodHandles.class.getMethod("lookup");
        return ((MethodHandles.Lookup) lk.invoke(null)).lookupClass().getName();
    }

    /** {@code Class.forName} reached through {@code Method.invoke} from this class. */
    static String reflectiveForName() throws Throwable {
        Method fn = Class.class.getMethod("forName", String.class);
        return ((Class<?>) fn.invoke(null, "SbrW6Target")).getName();
    }
}

/**
 * SBR wave 6, lane {@code dispatch}: {@code Reflection.getCallerClass} answers, observed through
 * the JDK's own caller-sensitive APIs (user code cannot call it: HotSpot demands
 * {@code @CallerSensitive} on the calling method). A private member reached reflectively from a
 * class that is not its nest host throws {@code IllegalAccessException} whose message NAMES THE
 * CALLER ("class X cannot access a member of class Y ..."), and {@code MethodHandles.lookup()} and
 * {@code StackWalker.getCallerClass()} return it. Every scenario prints its first-run value and is
 * re-run in a hot loop; a mismatch is counted as {@code drift}, so cold == warm is asserted.
 * Scenarios cover: direct call, lambda, reflection frames between (HotSpot skips
 * {@code Method.invoke} frames), a MethodHandle between, stream plumbing between, the caller-sensitive
 * method itself reached reflectively, and deep stacks.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . SbrW6Caller [hotIters]
 * </pre>
 */
public class SbrW6Caller {
    interface Scenario {
        String run() throws Throwable;
    }

    static final List<String> names = new ArrayList<>();
    static final List<Scenario> scenarios = new ArrayList<>();
    static int drift;

    static void add(String name, Scenario s) {
        names.add(name);
        scenarios.add(s);
    }

    static String msg(Throwable t) {
        Throwable c = t;
        while (c instanceof java.lang.reflect.InvocationTargetException && c.getCause() != null) c = c.getCause();
        String m = String.valueOf(c.getMessage());
        int i = m.indexOf(" (in module");
        while (i > 0) {
            int j = m.indexOf(')', i);
            m = m.substring(0, i) + (j > 0 ? m.substring(j + 1) : "");
            i = m.indexOf(" (in module");
        }
        return c.getClass().getSimpleName() + ": " + m;
    }

    static int depth(int n, Supplier<String> leaf, String[] out) {
        if (n == 0) {
            out[0] = leaf.get();
            return 0;
        }
        return 1 + depth(n - 1, leaf, out);
    }

    public static void main(String[] args) throws Throwable {
        int hot = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        Method mInvoke = Method.class.getMethod("invoke", Object.class, Object[].class);
        Method outsiderMethod = SbrW6Outsider.class.getDeclaredMethod("viaMethod");
        outsiderMethod.setAccessible(true);
        Method outsiderLookup = SbrW6Outsider.class.getDeclaredMethod("lookupClass");
        outsiderLookup.setAccessible(true);
        MethodHandles.Lookup me = MethodHandles.lookup();
        MethodHandle mhLk = me.findStatic(MethodHandles.class, "lookup", MethodType.methodType(MethodHandles.Lookup.class));
        MethodHandle mhFn = me.findStatic(Class.class, "forName", MethodType.methodType(Class.class, String.class));
        StackWalker sw = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

        add("lookup direct", () -> MethodHandles.lookup().lookupClass().getName());
        add("lookup outsider", () -> SbrW6Outsider.lookupClass());
        add("lookup in lambda", () -> ((Supplier<String>) () -> MethodHandles.lookup().lookupClass().getName()).get());
        add("lookup via stream map", () -> Stream.of(1).map(i -> MethodHandles.lookup().lookupClass().getName()).findFirst().get());
        add("lookup depth 40", () -> {
            String[] r = new String[1];
            depth(40, () -> MethodHandles.lookup().lookupClass().getName(), r);
            return r[0];
        });
        add("lookup via Method.invoke of outsider", () -> String.valueOf(outsiderLookup.invoke(null)));
        add("lookup reflective (Method.invoke)", () -> SbrW6Outsider.reflectiveLookup());
        add("lookup via MethodHandle.invokeExact", () -> ((MethodHandles.Lookup) mhLk.invokeExact()).lookupClass().getName());
        add("forName reflective (Method.invoke)", () -> SbrW6Outsider.reflectiveForName());
        add("forName via MethodHandle", () -> ((Class<?>) mhFn.invokeExact("SbrW6Target")).getName());
        add("outsider private method", () -> SbrW6Outsider.viaMethod());
        add("outsider private static method", () -> SbrW6Outsider.viaMethodStatic());
        add("outsider private ctor", () -> SbrW6Outsider.viaCtor());
        add("outsider private field get", () -> SbrW6Outsider.viaField());
        add("outsider private static field set", () -> SbrW6Outsider.viaStaticField());
        add("outsider method in lambda", () -> ((Supplier<String>) () -> SbrW6Outsider.viaMethod()).get());
        add("outsider method via Method.invoke", () -> String.valueOf(outsiderMethod.invoke(null)));
        add("outsider method via Method.invoke.invoke", () -> String.valueOf(mInvoke.invoke(outsiderMethod, null, new Object[0])));
        add("outsider method via stream", () -> Stream.of(1).map(i -> SbrW6Outsider.viaMethod()).findFirst().get());
        add("outsider method via forEach", () -> {
            String[] r = new String[1];
            List.of(1).forEach(i -> r[0] = SbrW6Outsider.viaMethod());
            return r[0];
        });
        add("outsider method depth 40", () -> {
            String[] r = new String[1];
            depth(40, () -> SbrW6Outsider.viaMethod(), r);
            return r[0];
        });
        add("sw.getCallerClass direct", () -> SbrW6Outsider.walkerCaller(sw));
        add("sw.getCallerClass in lambda", () -> ((Supplier<String>) () -> SbrW6Outsider.walkerCaller(sw)).get());
        add("sw.getCallerClass depth 30", () -> {
            String[] r = new String[1];
            depth(30, () -> SbrW6Outsider.walkerCaller(sw), r);
            return r[0];
        });
        add("class.forName direct", () -> {
            try {
                return Class.forName("SbrW6Target").getName();
            } catch (Throwable t) {
                return msg(t);
            }
        });
        add("thread runnable outsider", () -> {
            String[] r = new String[1];
            Thread t = new Thread(() -> r[0] = SbrW6Outsider.viaMethod());
            t.start();
            t.join();
            return r[0];
        });
        add("thread lookup", () -> {
            String[] r = new String[1];
            Thread t = new Thread(() -> r[0] = MethodHandles.lookup().lookupClass().getName());
            t.start();
            t.join();
            return r[0];
        });

        String[] first = new String[scenarios.size()];
        for (int i = 0; i < scenarios.size(); i++) {
            try {
                first[i] = scenarios.get(i).run();
            } catch (Throwable t) {
                first[i] = "THROW " + msg(t);
            }
            System.out.println("row " + names.get(i) + " = " + first[i]);
        }
        int[] drifts = new int[scenarios.size()];
        for (int it = 0; it < hot; it++) {
            for (int i = 0; i < scenarios.size(); i++) {
                if (names.get(i).startsWith("thread") && it % 50 != 0) continue;
                String v;
                try {
                    v = scenarios.get(i).run();
                } catch (Throwable t) {
                    v = "THROW " + msg(t);
                }
                if (!v.equals(first[i])) {
                    drifts[i]++;
                    drift++;
                }
            }
        }
        for (int i = 0; i < scenarios.size(); i++) {
            if (drifts[i] != 0) System.out.println("drift " + names.get(i) + " x" + drifts[i]);
        }
        System.out.println("drift " + drift);
    }
}
