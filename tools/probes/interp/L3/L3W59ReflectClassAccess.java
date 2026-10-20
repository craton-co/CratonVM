// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-reflective-doors-skip-the-class-accessibility-check-FIXED-20261023.md
// (switch CRATONVM_REFLECT_CLASS_ACCESS): `Reflection.verifyMemberAccess`'s
// class half on every served reflective door. A child-first loader defines
// `L3W59RcaHidden` (package-private) in the caller's package NAME, so it
// is in another RUNTIME package: every member, public or not, is refused
// without `setAccessible`. The rows that must keep working: the override
// (and the Spring/JUnit `makeAccessible` idiom, which sets it exactly when the
// member OR its class is not public), a package-private class of the caller's
// own runtime package, and a PUBLIC nested class of the hidden class (its class
// FILE flags are public). A null receiver on an instance method is HotSpot's
// `NullPointerException`, ahead of every access refusal (member, class or
// module; no switch -- the `*-null-receiver` rows). The first row is repeated
// last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// method: java.lang.IllegalAccessException class L3W59ReflectClassAccess cannot access a member of class L3W59RcaHidden with modifiers "public static"
// method-instance: java.lang.IllegalAccessException class L3W59ReflectClassAccess cannot access a member of class L3W59RcaHidden with modifiers "public"
// method-null-receiver: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "obj" is null
// field-get: java.lang.IllegalAccessException class L3W59ReflectClassAccess cannot access a member of class L3W59RcaHidden with modifiers "public static"
// field-set: java.lang.IllegalAccessException class L3W59ReflectClassAccess cannot access a member of class L3W59RcaHidden with modifiers "public static"
// field-instance: java.lang.IllegalAccessException class L3W59ReflectClassAccess cannot access a member of class L3W59RcaHidden with modifiers "public"
// ctor: java.lang.IllegalAccessException class L3W59ReflectClassAccess cannot access a member of class L3W59RcaHidden with modifiers "public"
// class-new-instance: java.lang.IllegalAccessException class L3W59ReflectClassAccess cannot access a member of class L3W59RcaHidden with modifiers "public"
// accessible-method: 7
// accessible-field: 7
// accessible-ctor: L3W59RcaHidden
// can-access: false false false true
// make-accessible-idiom: 7
// same-runtime-package: 5
// public-nested: 9
// private-null-receiver: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "obj" is null
// private-receiver: java.lang.IllegalAccessException class L3W59ReflectClassAccess cannot access a member of class L3W59RcaLocal with modifiers "private"
// module-null-receiver: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "obj" is null
// module-receiver: java.lang.IllegalAccessException
// method-again: java.lang.IllegalAccessException class L3W59ReflectClassAccess cannot access a member of class L3W59RcaHidden with modifiers "public static"
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W59ReflectClassAccess`.
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;

class L3W59RcaHidden {
    public static int f = 7;
    public int g = 7;

    public L3W59RcaHidden() {
    }

    public static int m() {
        return 7;
    }

    public int im() {
        return 7;
    }

    public static class Pub {
        public static int n() {
            return 9;
        }
    }
}

class L3W59RcaLocal {
    public static int m() {
        return 5;
    }

    private int secret() {
        return 3;
    }
}

public class L3W59ReflectClassAccess {
    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (!name.startsWith("L3W59RcaHidden")) {
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

    // Spring's `ReflectionUtils.makeAccessible(Method)`, JUnit's
    // `ReflectionUtils.makeAccessible`: override exactly when needed.
    static void makeAccessible(Method m) {
        if ((!Modifier.isPublic(m.getModifiers())
                || !Modifier.isPublic(m.getDeclaringClass().getModifiers()))
                && !m.canAccess(Modifier.isStatic(m.getModifiers()) ? null : new Object())) {
            m.setAccessible(true);
        }
    }

    @SuppressWarnings("deprecation")
    public static void main(String[] args) throws Exception {
        ClassLoader cf = new ChildFirst(L3W59ReflectClassAccess.class.getClassLoader());
        Class<?> h = Class.forName("L3W59RcaHidden", true, cf);
        Object inst = h.getMethod("m") != null ? newAccessible(h) : null;
        Method m = h.getMethod("m");
        Method im = h.getMethod("im");
        Field f = h.getField("f");
        Field g = h.getField("g");
        row("method", () -> m.invoke(null));
        row("method-instance", () -> im.invoke(inst));
        row("method-null-receiver", () -> im.invoke(null));
        row("field-get", () -> f.get(null));
        row("field-set", () -> {
            f.set(null, 8);
            return null;
        });
        row("field-instance", () -> g.get(inst));
        row("ctor", () -> h.getConstructor().newInstance());
        row("class-new-instance", () -> h.newInstance());
        Method m2 = h.getMethod("m");
        m2.setAccessible(true);
        row("accessible-method", () -> m2.invoke(null));
        Field f2 = h.getField("f");
        f2.setAccessible(true);
        row("accessible-field", () -> f2.get(null));
        Constructor<?> c2 = h.getConstructor();
        c2.setAccessible(true);
        row("accessible-ctor", () -> c2.newInstance().getClass().getName());
        row("can-access", () -> m.canAccess(null) + " " + f.canAccess(null) + " "
                + h.getConstructor().canAccess(null) + " "
                + L3W59RcaLocal.class.getMethod("m").canAccess(null));
        Method m3 = h.getMethod("m");
        makeAccessible(m3);
        row("make-accessible-idiom", () -> m3.invoke(null));
        row("same-runtime-package", () -> L3W59RcaLocal.class.getMethod("m").invoke(null));
        Class<?> pub = Class.forName("L3W59RcaHidden$Pub", true, cf);
        row("public-nested", () -> pub.getMethod("n").invoke(null));
        // A null receiver outranks every refusal (`obj.getClass()` builds
        // `checkAccess`'s arguments): a denied private method, and a public
        // method of a package java.base does not export.
        Method secret = L3W59RcaLocal.class.getDeclaredMethod("secret");
        row("private-null-receiver", () -> secret.invoke(null));
        row("private-receiver", () -> secret.invoke(new L3W59RcaLocal()));
        Method findResource = Class.forName("jdk.internal.loader.BuiltinClassLoader")
                .getMethod("findResource", String.class);
        row("module-null-receiver", () -> findResource.invoke(null, "x"));
        rowType("module-receiver", () -> findResource.invoke(ClassLoader.getSystemClassLoader(), "x"));
        row("method-again", () -> m.invoke(null));
    }

    static void rowType(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            System.out.println(name + ": " + t.getClass().getName());
        }
    }

    static Object newAccessible(Class<?> h) throws Exception {
        Constructor<?> c = h.getConstructor();
        c.setAccessible(true);
        return c.newInstance();
    }
}
