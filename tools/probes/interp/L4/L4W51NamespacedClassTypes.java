// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 51, lane L4: NON-array type checks between two
// loaders' copies of one class name (page
// `docs/internal/fixed-bugs/interpreter-L4-compatible-type-checks-keep-the-by-name-loader-rule-FIXED-20261016.md`;
// alignment row `namespaced-class-types`).
//
// A child-first loader defines `X`, `I` and `Checker` a second time. The
// child's `Checker` does `checkcast X` / `instanceof X` / `instanceof I`,
// which resolve to the CHILD's classes; the app's objects are handed to it.
// `--jdk-only` matches HotSpot, and so does `--compatible` since wave 52 (the
// row is on by default); with the row off it admits each cross-loader row by
// NAME:
//
//   CRATONVM_COMPAT_ALIGN=-namespaced-class-types cratonvm --compatible ...
//
// The first row runs on a cold VM; `cast-again` repeats it last.
//
// Run: javac -d out L4W51NamespacedClassTypes.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W51NamespacedClassTypes
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   cast: java.lang.ClassCastException
//   instanceof: false
//   instanceof-iface: false
//   proxy-iface: false
//   is-instance: false
//   is-assignable: false
//   is-assignable-iface: false
//   own-cast: ok
//   own-instanceof: true
//   hot-instanceof: 0
//   cast-again: java.lang.ClassCastException
import java.io.InputStream;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;

public class L4W51NamespacedClassTypes {
    public interface I {
    }

    public static class X implements I {
    }

    public static class Checker {
        public static Object cast(Object o) {
            return (X) o;
        }

        public static boolean isX(Object o) {
            return o instanceof X;
        }

        public static boolean isI(Object o) {
            return o instanceof I;
        }

        public static int hot(Object o) {
            int n = 0;
            for (int i = 0; i < 20000; i++) {
                if (o instanceof X) {
                    n++;
                }
            }
            return n;
        }
    }

    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) {
            super("child", parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.startsWith(L4W51NamespacedClassTypes.class.getName() + "$")
                    || name.endsWith("$ChildFirst")) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    try (InputStream in = getParent().getResourceAsStream(name.replace('.', '/') + ".class")) {
                        byte[] b = in.readAllBytes();
                        c = defineClass(name, b, 0, b.length);
                    } catch (java.io.IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return c;
            }
        }
    }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (java.lang.reflect.InvocationTargetException e) {
            out = e.getCause().getClass().getName();
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Exception {
        ClassLoader child = new ChildFirst(L4W51NamespacedClassTypes.class.getClassLoader());
        Class<?> checker = child.loadClass(Checker.class.getName());
        Class<?> childX = child.loadClass(X.class.getName());
        Class<?> childI = child.loadClass(I.class.getName());
        Method cast = checker.getMethod("cast", Object.class);
        Method isX = checker.getMethod("isX", Object.class);
        Method isI = checker.getMethod("isI", Object.class);
        Method hot = checker.getMethod("hot", Object.class);
        X appX = new X();
        Object childValue = childX.getDeclaredConstructor().newInstance();

        row("cast", () -> {
            cast.invoke(null, appX);
            return "ok";
        });
        row("instanceof", () -> isX.invoke(null, appX));
        row("instanceof-iface", () -> isI.invoke(null, appX));
        row("proxy-iface", () -> {
            Object p = Proxy.newProxyInstance(L4W51NamespacedClassTypes.class.getClassLoader(),
                new Class<?>[] {I.class}, (proxy, m, a) -> null);
            return isI.invoke(null, p);
        });
        row("is-instance", () -> childX.isInstance(appX));
        row("is-assignable", () -> childX.isAssignableFrom(X.class));
        row("is-assignable-iface", () -> childI.isAssignableFrom(X.class));
        row("own-cast", () -> {
            cast.invoke(null, childValue);
            return "ok";
        });
        row("own-instanceof", () -> isX.invoke(null, childValue));
        row("hot-instanceof", () -> hot.invoke(null, appX));
        row("cast-again", () -> {
            cast.invoke(null, appX);
            return "ok";
        });
    }
}
