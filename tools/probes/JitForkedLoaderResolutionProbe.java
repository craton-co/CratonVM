// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.reflect.Method;

/**
 * JIT-compiled code in a child-first ("forked") class loader must resolve a
 * constant-pool class reference through THAT loader, even when the compiler
 * saw only the application's copy of the name.
 *
 * <p>The application loads {@code Target}, {@code StatOwner} and
 * {@code Marker} first. A child-first loader then redefines every nested
 * class of this probe, and its {@code ForkCode} methods are warmed with the
 * branch that touches those names NOT taken -- so whatever compiles them sees
 * only the application's copies. Only then are the branches taken. On
 * HotSpot every answer is the fork's: the fork's {@code loadClass} is what a
 * constant-pool resolution in fork code runs, and it defines its own copy.
 *
 * <p>Run it with the JIT on and off; both must print what HotSpot prints.
 * {@code docs/internal/fixed-suite-bugs/spring/
 * spring-jdkonly-bytecode-cast-name-rule-jit-class-resolution-FIXED-20260923.md}.
 */
public class JitForkedLoaderResolutionProbe {
    public interface Marker {
    }

    public static class Target implements Marker {
        public static int stat() {
            return 1;
        }
    }

    public static class StatOwner {
        public static ClassLoader owner() {
            return StatOwner.class.getClassLoader();
        }
    }

    /** Only ever loaded by the fork. Every method's interesting branch is cold while warming. */
    public static class ForkCode {
        public static Object make(boolean hot) {
            if (hot) {
                return new Target();
            }
            return null;
        }

        public static boolean isTarget(Object o, boolean hot) {
            if (hot) {
                return o instanceof Target;
            }
            return false;
        }

        public static boolean isMarker(Object o, boolean hot) {
            if (hot) {
                return o instanceof Marker;
            }
            return false;
        }

        public static String cast(Object o, boolean hot) {
            if (hot) {
                try {
                    Target t = (Target) o;
                    return "ok " + (t != null);
                } catch (ClassCastException e) {
                    return "CCE";
                }
            }
            return null;
        }

        public static ClassLoader statOwner(boolean hot) {
            if (hot) {
                return StatOwner.owner();
            }
            return null;
        }

        public static Object makeArray(boolean hot) {
            if (hot) {
                return new Target[1];
            }
            return null;
        }

        public static Object classLiteral(boolean hot) {
            if (hot) {
                return Target.class;
            }
            return null;
        }

        /** Warm every method with the cold branch, in fork code, so the JIT compiles them. */
        public static int warm(int n) {
            int acc = 0;
            for (int i = 0; i < n; i++) {
                acc += make(false) == null ? 1 : 0;
                acc += isTarget(null, false) ? 0 : 1;
                acc += isMarker(null, false) ? 0 : 1;
                acc += cast(null, false) == null ? 1 : 0;
                acc += statOwner(false) == null ? 1 : 0;
                acc += makeArray(false) == null ? 1 : 0;
                acc += classLiteral(false) == null ? 1 : 0;
            }
            return acc;
        }
    }

    static final class Fork extends ClassLoader {
        Fork(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.startsWith(JitForkedLoaderResolutionProbe.class.getName() + "$")) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    byte[] b = bytes(name);
                    c = defineClass(name, b, 0, b.length);
                }
                return c;
            }
        }

        private byte[] bytes(String name) throws ClassNotFoundException {
            String res = name.replace('.', '/') + ".class";
            try (InputStream in = getParent().getResourceAsStream(res)) {
                if (in == null) {
                    throw new ClassNotFoundException(name);
                }
                ByteArrayOutputStream out = new ByteArrayOutputStream();
                in.transferTo(out);
                return out.toByteArray();
            } catch (java.io.IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }
    }

    static void row(String name, Object value) {
        System.out.println(name + " = " + value);
    }

    public static void main(String[] args) throws Throwable {
        int warm = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        ClassLoader app = JitForkedLoaderResolutionProbe.class.getClassLoader();
        // The application's copies exist before the fork code is compiled.
        Object appTarget = new Target();
        row("app StatOwner.owner() is app", StatOwner.owner() == app);
        row("app Target.stat()", Target.stat());

        Fork fork = new Fork(app);
        Class<?> code = fork.loadClass(ForkCode.class.getName());
        row("ForkCode defined by fork", code.getClassLoader() == fork);
        Method warmM = code.getMethod("warm", int.class);
        row("warm", warmM.invoke(null, warm));

        Object made = code.getMethod("make", boolean.class).invoke(null, true);
        row("fork new Target() defined by fork", made.getClass().getClassLoader() == fork);
        Class<?> forkTarget = fork.loadClass(Target.class.getName());
        row("fork loadClass(Target) is the made class", forkTarget == made.getClass());

        Method isTarget = code.getMethod("isTarget", Object.class, boolean.class);
        row("fork code: app Target instanceof Target", isTarget.invoke(null, appTarget, true));
        row("fork code: fork Target instanceof Target", isTarget.invoke(null, made, true));

        Method isMarker = code.getMethod("isMarker", Object.class, boolean.class);
        row("fork code: app Target instanceof Marker", isMarker.invoke(null, appTarget, true));
        row("fork code: fork Target instanceof Marker", isMarker.invoke(null, made, true));

        Method cast = code.getMethod("cast", Object.class, boolean.class);
        row("fork code: (Target) appTarget", cast.invoke(null, appTarget, true));
        row("fork code: (Target) forkTarget", cast.invoke(null, made, true));

        row("fork code: StatOwner.owner() is fork",
                code.getMethod("statOwner", boolean.class).invoke(null, true) == fork);
        Object arr = code.getMethod("makeArray", boolean.class).invoke(null, true);
        row("fork code: new Target[1] component is fork's",
                arr.getClass().getComponentType() == forkTarget);
        row("fork code: Target.class is fork's",
                code.getMethod("classLiteral", boolean.class).invoke(null, true) == forkTarget);

        // The application's own view is untouched.
        row("app: fork Target instanceof app Target", made instanceof Target);
        row("app: app Target instanceof app Target", appTarget instanceof Target);
        System.out.println("PROBE_DONE");
    }
}
