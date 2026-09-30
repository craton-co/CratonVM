// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 32 (orchestrator): a `checkcast` failure names
// each operand's own loader, as HotSpot does, when a user-defined loader is
// involved
// (docs/internal/fixed-bugs/interpreter-L4-cross-loader-type-checks-and-trace-shapes-FIXED-20261001.md,
// item 3).
//
// A child-first loader defines `X` and `Caster` a second time; the child's
// `Caster.cast` does `checkcast X`, which resolves to the CHILD's `X`.
//
//   two-loaders  the app's `X` cast to the child's `X`
//   named-child  an `Integer` cast to the child's `X` (loader named "child")
//   unnamed      the same with a loader constructed without a name (HotSpot
//                names it by its class)
//
// A user loader's clause ends in `@<identity hash>`, which differs run to run,
// so the probe prints `@<hash>`.
//
// Before wave 32 CratonVM credited both operands of the first row to 'app'
// (each operand was resolved by name), and printed the bare
// `X cannot be cast to Y` for the other two (a user loader had no name).
// `--compatible` keeps that wording.
//
// Run: javac -d out L4W32CastLoaderText.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W32CastLoaderText
//
// Expected HotSpot 25 output (default and -Xint):
//   two-loaders: class L4W32CastLoaderText$X cannot be cast to class L4W32CastLoaderText$X (L4W32CastLoaderText$X is in unnamed module of loader 'app'; L4W32CastLoaderText$X is in unnamed module of loader 'child' @<hash>)
//   named-child: class java.lang.Integer cannot be cast to class L4W32CastLoaderText$X (java.lang.Integer is in module java.base of loader 'bootstrap'; L4W32CastLoaderText$X is in unnamed module of loader 'child' @<hash>)
//   unnamed: class java.lang.Integer cannot be cast to class L4W32CastLoaderText$X (java.lang.Integer is in module java.base of loader 'bootstrap'; L4W32CastLoaderText$X is in unnamed module of loader L4W32CastLoaderText$ChildFirst @<hash>)
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;

public class L4W32CastLoaderText {
    public static class X {
    }

    public static class Caster {
        public static Object cast(Object o) {
            return (X) o;
        }
    }

    static final class ChildFirst extends ClassLoader {
        ChildFirst(String name) {
            super(name, L4W32CastLoaderText.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (!name.startsWith("L4W32CastLoaderText$") || name.contains("ChildFirst")) {
                    return super.loadClass(name, resolve);
                }
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                try (InputStream in = getParent().getResourceAsStream(name.replace('.', '/') + ".class")) {
                    byte[] b = in.readAllBytes();
                    return defineClass(name, b, 0, b.length);
                } catch (Exception e) {
                    throw new ClassNotFoundException(name, e);
                }
            }
        }
    }

    static String cast(ClassLoader loader, Object value) {
        try {
            Class<?> caster = loader.loadClass("L4W32CastLoaderText$Caster");
            Object r = caster.getMethod("cast", Object.class).invoke(null, value);
            return "cast succeeded: " + r;
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            return String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@<hash>");
        } catch (Throwable t) {
            return t.toString();
        }
    }

    public static void main(String[] args) {
        System.out.println("two-loaders: " + cast(new ChildFirst("child"), new X()));
        System.out.println("named-child: " + cast(new ChildFirst("child"), 7));
        System.out.println("unnamed: " + cast(new ChildFirst(null), 7));
    }
}
