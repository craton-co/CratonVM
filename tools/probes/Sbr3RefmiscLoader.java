// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.reflect.Method;
import java.util.concurrent.Callable;

/**
 * SBR round 3 wave 2, lane {@code refmisc}: what {@code Class.classLoader} (the raw field the
 * JDK's {@code getClassLoader0} and {@code getPackage} read) holds, per kind of class. HotSpot:
 * {@code null} for boot classes, the platform loader for platform classes, the application loader
 * for application classes, a lambda's host loader for the hidden lambda class, the element's
 * loader for arrays.
 *
 * <pre>
 *   cratonvm --jdk-only --add-opens java.base/java.lang=ALL-UNNAMED -cp . Sbr3RefmiscLoader
 * </pre>
 */
public final class Sbr3RefmiscLoader {
    interface Hook {
        void run();
    }

    static Method raw;

    static String show(Class<?> c) throws Exception {
        Object l = raw.invoke(c);
        return c.getName().replaceAll("0x[0-9a-f]+", "0xN") + " -> " + (l == null ? "null" : l.getClass().getName() + "/" + ((ClassLoader) l).getName());
    }

    static void row(String name, Callable<Object> s) {
        try {
            System.out.println(name + " = " + s.call());
        } catch (Throwable t) {
            System.out.println(name + " threw " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        raw = Class.class.getDeclaredMethod("getClassLoader0");
        raw.setAccessible(true);
        row("boot String", () -> show(String.class));
        row("boot array", () -> show(String[].class));
        row("platform java.sql", () -> show(java.sql.Driver.class));
        row("app self", () -> show(Sbr3RefmiscLoader.class));
        row("app array", () -> show(Sbr3RefmiscLoader[].class));
        row("app nested", () -> show(Hook.class));
        row("lambda", () -> show(((Hook) () -> {}).getClass()));
        row("lambda twice", () -> show(((Runnable) () -> {}).getClass()));
        row("proxy", () -> show(java.lang.reflect.Proxy.newProxyInstance(Sbr3RefmiscLoader.class.getClassLoader(),
                new Class<?>[] {Runnable.class}, (p, m, a) -> null).getClass()));
        row("proxy jdk iface only", () -> show(java.lang.reflect.Proxy.newProxyInstance(null,
                new Class<?>[] {Runnable.class}, (p, m, a) -> null).getClass()));
        row("hidden", () -> {
            byte[] b = Sbr3RefmiscLoader.class.getResourceAsStream("Sbr3RefmiscLoader$Hook.class").readAllBytes();
            return show(MethodHandles.lookup().defineHiddenClass(b, false).lookupClass());
        });
        row("anonymous", () -> show(new Object() {}.getClass()));
        row("custom", () -> {
            ClassLoader cl = new ClassLoader("mine", null) {};
            return cl.getName();
        });
        row("getClassLoader app", () -> Sbr3RefmiscLoader.class.getClassLoader().getName());
        row("getClassLoader lambda", () -> ((Hook) () -> {}).getClass().getClassLoader().getName());
        row("getClassLoader String", () -> String.class.getClassLoader());
        row("module lambda", () -> ((Hook) () -> {}).getClass().getModule().isNamed());
        row("pkg lambda", () -> ((Hook) () -> {}).getClass().getPackage().getName().isEmpty());
        row("pkg hidden", () -> {
            byte[] b = Sbr3RefmiscLoader.class.getResourceAsStream("Sbr3RefmiscLoader$Hook.class").readAllBytes();
            return MethodHandles.lookup().defineHiddenClass(b, false).lookupClass().getPackage().getName().isEmpty();
        });
        System.out.println("PROBE-DONE");
    }
}
