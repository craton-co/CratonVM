// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L3 (review) --
// docs/internal/fixed-bugs/interpreter-L3-get-declaring-class-skips-the-inner-classes-agreement-check-FIXED-20261023.md:
// a nested class defined by a child-first loader whose outer class resolves
// to the APPLICATION loader's copy. HotSpot's `Class.getDeclaringClass0`
// (`InstanceKlass::compute_enclosing_class`, `Reflection::check_for_inner_class`)
// requires the outer class's `InnerClasses` to list THIS class, as resolved
// by the outer class's loader -- another `Class` object -- and throws
// `IncompatibleClassChangeError`. `getSimpleName` and `getCanonicalName`
// reach it through `getEnclosingClass`. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// declaring: java.lang.IncompatibleClassChangeError L3W58InnerClassesAgreement and L3W58InnerClassesAgreement$Nested disagree on InnerClasses attribute
// enclosing: java.lang.IncompatibleClassChangeError L3W58InnerClassesAgreement and L3W58InnerClassesAgreement$Nested disagree on InnerClasses attribute
// canonical: java.lang.IncompatibleClassChangeError L3W58InnerClassesAgreement and L3W58InnerClassesAgreement$Nested disagree on InnerClasses attribute
// same-loader: class L3W58InnerClassesAgreement
// declaring-again: java.lang.IncompatibleClassChangeError L3W58InnerClassesAgreement and L3W58InnerClassesAgreement$Nested disagree on InnerClasses attribute
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W58InnerClassesAgreement`.
import java.io.ByteArrayOutputStream;
import java.io.InputStream;

public class L3W58InnerClassesAgreement {
    public static class Nested {
    }

    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (!name.equals("L3W58InnerClassesAgreement$Nested")) {
                    return super.loadClass(name, resolve);
                }
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    try (InputStream in = getParent().getResourceAsStream(name + ".class")) {
                        ByteArrayOutputStream out = new ByteArrayOutputStream();
                        in.transferTo(out);
                        byte[] b = out.toByteArray();
                        c = defineClass(name, b, 0, b.length);
                    } catch (java.io.IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return c;
            }
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> n = Class.forName("L3W58InnerClassesAgreement$Nested", false,
                new ChildFirst(L3W58InnerClassesAgreement.class.getClassLoader()));
        row("declaring", n::getDeclaringClass);
        row("enclosing", n::getEnclosingClass);
        row("canonical", n::getCanonicalName);
        row("same-loader", Nested.class::getDeclaringClass);
        row("declaring-again", n::getDeclaringClass);
    }
}
