// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

package com.sun.cvmprobe;

import java.io.InputStream;

/**
 * A loader whose parent is the PLATFORM loader must define an application class itself, even
 * when the class's name starts with a JDK-looking prefix ({@code com.sun.}, {@code javax.}).
 *
 * <p>This probe lives in {@code com.sun.cvmprobe} on purpose: the name is JDK-shaped, the class
 * is not in the runtime image. On HotSpot the platform loader cannot see it and the child's own
 * {@code findClass} defines a second copy -- the shape of Spring's
 * {@code CompileWithForkedClassLoaderClassLoader} loading GlassFish's
 * {@code com.sun.el.ExpressionFactoryImpl}.
 *
 * <pre>
 *   javac -d /tmp/p tools/probes/PlatformParentForkProbe.java
 *   java -cp /tmp/p com.sun.cvmprobe.PlatformParentForkProbe
 * </pre>
 *
 * {@code docs/internal/fixed-suite-bugs/spring/
 * spring-jdkonly-bytecode-cast-name-rule-jit-class-resolution-FIXED-20260923.md}.
 */
public class PlatformParentForkProbe {
    public interface Api {
        String who();
    }

    public static class Impl implements Api {
        public String who() {
            return "impl";
        }
    }

    static final class Fork extends ClassLoader {
        final ClassLoader app;
        int findClassCalls;

        Fork(ClassLoader app) {
            super(app.getParent());
            this.app = app;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            findClassCalls++;
            try (InputStream in = app.getResourceAsStream(name.replace('.', '/') + ".class")) {
                if (in == null) {
                    throw new ClassNotFoundException(name);
                }
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length);
            } catch (java.io.IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }
    }

    static void row(String k, Object v) {
        System.out.println(k + " = " + v);
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = PlatformParentForkProbe.class.getClassLoader();
        String impl = Impl.class.getName();
        String api = Api.class.getName();
        try {
            app.getParent().loadClass(impl);
            row("platform.loadClass(Impl)", "found");
        } catch (ClassNotFoundException e) {
            row("platform.loadClass(Impl)", "ClassNotFoundException");
        }
        Fork fork = new Fork(app);
        Class<?> c = fork.loadClass(impl);
        row("fork.loadClass(Impl) defined by fork", c.getClassLoader() == fork);
        row("fork findClass ran", fork.findClassCalls > 0);
        Class<?> forkApi = fork.loadClass(api);
        row("fork Api defined by fork", forkApi.getClassLoader() == fork);
        row("fork Impl implements fork Api", forkApi.isAssignableFrom(c));
        row("fork Impl implements app Api", Api.class.isAssignableFrom(c));
        Object o = c.getDeclaredConstructor().newInstance();
        row("(fork Api) forkImpl", forkApi.cast(o) != null);
        try {
            Api a = (Api) o;
            row("(app Api) forkImpl", "ok " + a.who());
        } catch (ClassCastException e) {
            row("(app Api) forkImpl", "ClassCastException");
        }
        Class<?> byName = Class.forName(impl, false, fork);
        row("Class.forName(Impl, fork) is the fork's", byName == c);
        System.out.println("PROBE_DONE");
    }
}
