// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-get-declaring-class-skips-the-inner-classes-agreement-check-FIXED-20261023.md
// (switch CRATONVM_INNER_CLASSES_AGREEMENT): HotSpot's
// `Reflection::check_for_inner_class` on every door that answers a member
// class's declaring class. A child-first loader defines only the nested
// class; its outer class resolves to the application loader's copy, whose
// `InnerClasses`, resolved by THAT loader, names the application loader's
// nested class -- another `Class` -- so `IncompatibleClassChangeError`.
// `getSimpleName` asks it too (`isTopLevelClass` -> `getDeclaringClass0`).
// Controls: the application loader's own nested class, and a loader that
// defines the outer AND the nested class (they agree). The first row is
// repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// declaring: java.lang.IncompatibleClassChangeError L3W59InnerClassesAgreement and L3W59InnerClassesAgreement$Nested disagree on InnerClasses attribute
// enclosing: java.lang.IncompatibleClassChangeError L3W59InnerClassesAgreement and L3W59InnerClassesAgreement$Nested disagree on InnerClasses attribute
// simple: java.lang.IncompatibleClassChangeError L3W59InnerClassesAgreement and L3W59InnerClassesAgreement$Nested disagree on InnerClasses attribute
// canonical: java.lang.IncompatibleClassChangeError L3W59InnerClassesAgreement and L3W59InnerClassesAgreement$Nested disagree on InnerClasses attribute
// member: java.lang.IncompatibleClassChangeError L3W59InnerClassesAgreement and L3W59InnerClassesAgreement$Nested disagree on InnerClasses attribute
// same-loader: class L3W59InnerClassesAgreement Nested L3W59InnerClassesAgreement.Nested
// both-child-first: class L3W59InnerClassesAgreement true Nested
// declaring-again: java.lang.IncompatibleClassChangeError L3W59InnerClassesAgreement and L3W59InnerClassesAgreement$Nested disagree on InnerClasses attribute
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W59InnerClassesAgreement`.
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.util.Set;

public class L3W59InnerClassesAgreement {
    public static class Nested {
    }

    static final class ChildFirst extends ClassLoader {
        private final Set<String> mine;

        ChildFirst(ClassLoader parent, Set<String> mine) {
            super(parent);
            this.mine = mine;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (!mine.contains(name)) {
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
        String outer = "L3W59InnerClassesAgreement";
        String nested = outer + "$Nested";
        ClassLoader app = L3W59InnerClassesAgreement.class.getClassLoader();
        Class<?> n = Class.forName(nested, false, new ChildFirst(app, Set.of(nested)));
        row("declaring", n::getDeclaringClass);
        row("enclosing", n::getEnclosingClass);
        row("simple", n::getSimpleName);
        row("canonical", n::getCanonicalName);
        row("member", n::isMemberClass);
        row("same-loader", () -> Nested.class.getDeclaringClass() + " "
                + Nested.class.getSimpleName() + " " + Nested.class.getCanonicalName());
        Class<?> both = Class.forName(nested, false, new ChildFirst(app, Set.of(outer, nested)));
        row("both-child-first", () -> both.getDeclaringClass() + " "
                + (both.getDeclaringClass() != L3W59InnerClassesAgreement.class) + " "
                + both.getSimpleName());
        row("declaring-again", n::getDeclaringClass);
    }
}
