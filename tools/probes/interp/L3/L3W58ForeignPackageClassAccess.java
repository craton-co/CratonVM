// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L3 (review) --
// docs/internal/fixed-bugs/interpreter-L3-reflective-doors-skip-the-class-accessibility-check-FIXED-20261023.md:
// a PUBLIC member of a package-private class, reached reflectively from
// another runtime package (the same package name, another loader), without
// `setAccessible`. HotSpot's `Reflection.verifyMemberAccess` first asks
// whether the CLASS is accessible, so every door refuses with an
// `IllegalAccessException` naming the class's kind and the modifiers. A
// child-first loader defines the class. The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// method: java.lang.IllegalAccessException class L3W58ForeignPackageClassAccess cannot access a member of class L3W58FpcaHidden with modifiers "public static"
// field-get: java.lang.IllegalAccessException class L3W58ForeignPackageClassAccess cannot access a member of class L3W58FpcaHidden with modifiers "public static"
// field-set: java.lang.IllegalAccessException class L3W58ForeignPackageClassAccess cannot access a member of class L3W58FpcaHidden with modifiers "public static"
// ctor: java.lang.IllegalAccessException class L3W58ForeignPackageClassAccess cannot access a member of class L3W58FpcaHidden with modifiers "public"
// accessible-method: 7
// method-again: java.lang.IllegalAccessException class L3W58ForeignPackageClassAccess cannot access a member of class L3W58FpcaHidden with modifiers "public static"
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W58ForeignPackageClassAccess`.
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

class L3W58FpcaHidden {
    public static int f = 7;

    public L3W58FpcaHidden() {
    }

    public static int m() {
        return 7;
    }
}

public class L3W58ForeignPackageClassAccess {
    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (!name.startsWith("L3W58Fpca")) {
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
        Class<?> h = Class.forName("L3W58FpcaHidden", true, new ChildFirst(L3W58ForeignPackageClassAccess.class.getClassLoader()));
        Method m = h.getMethod("m");
        Field f = h.getField("f");
        row("method", () -> m.invoke(null));
        row("field-get", () -> f.get(null));
        row("field-set", () -> {
            f.set(null, 8);
            return null;
        });
        row("ctor", () -> h.getConstructor().newInstance());
        Method m2 = h.getMethod("m");
        m2.setAccessible(true);
        row("accessible-method", () -> m2.invoke(null));
        row("method-again", () -> m.invoke(null));
    }
}
