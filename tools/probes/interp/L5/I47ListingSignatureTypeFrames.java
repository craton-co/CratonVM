// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Item 3 of docs/internal/fixed-bugs/interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007.md
// (2026-10-07): the frames of the `NoClassDefFoundError` a reflective
// listing raises when a listed member's signature type is missing, through
// every listing door of `java.lang.Class`, with the missing type in a member
// of the class itself (`own`), of its superclass (`super`) or of an
// interface it implements (`iface`). HotSpot lists the JDK frames between
// the door and the native that resolves the types (`Reflection::new_*`
// inside `getDeclared{Methods,Fields,Constructors}0`).
//
// Each row prints `<door>-<where>: <throwable> [<frames>]`, the frames being
// the `java.lang.Class` methods above the probe's own first frame (the
// `(Native Method)` ones marked `(N)`), or `<door>-<where>: ok` when the
// listing does not fail. The holders are defined by a loader that refuses
// `I47ListingSignatureTypeFrames$Gone`.
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3, default flags and -Xint)
// getDeclaredMethods-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredMethods0(N) java.lang.Class.privateGetDeclaredMethods java.lang.Class.getDeclaredMethods]
// getDeclaredFields-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredFields0(N) java.lang.Class.privateGetDeclaredFields java.lang.Class.getDeclaredFields]
// getDeclaredConstructors-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredConstructors0(N) java.lang.Class.privateGetDeclaredConstructors java.lang.Class.getDeclaredConstructors]
// getDeclaredMethod-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredMethods0(N) java.lang.Class.privateGetDeclaredMethods java.lang.Class.getDeclaredMethod]
// getDeclaredField-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredFields0(N) java.lang.Class.privateGetDeclaredFields java.lang.Class.getDeclaredField]
// getDeclaredConstructor-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredConstructors0(N) java.lang.Class.privateGetDeclaredConstructors java.lang.Class.getConstructor0 java.lang.Class.getDeclaredConstructor]
// getMethods-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredMethods0(N) java.lang.Class.privateGetDeclaredMethods java.lang.Class.privateGetPublicMethods java.lang.Class.getMethods]
// getMethod-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredMethods0(N) java.lang.Class.privateGetDeclaredMethods java.lang.Class.getMethodsRecursive java.lang.Class.getMethod0 java.lang.Class.getMethod]
// getFields-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredFields0(N) java.lang.Class.privateGetDeclaredFields java.lang.Class.privateGetPublicFields java.lang.Class.getFields]
// getField-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredFields0(N) java.lang.Class.privateGetDeclaredFields java.lang.Class.getField0 java.lang.Class.getField]
// getConstructors-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredConstructors0(N) java.lang.Class.privateGetDeclaredConstructors java.lang.Class.getConstructors]
// getConstructor-own: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredConstructors0(N) java.lang.Class.privateGetDeclaredConstructors java.lang.Class.getConstructor0 java.lang.Class.getConstructor]
// getMethods-super: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredMethods0(N) java.lang.Class.privateGetDeclaredMethods java.lang.Class.privateGetPublicMethods java.lang.Class.privateGetPublicMethods java.lang.Class.getMethods]
// getMethod-super: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredMethods0(N) java.lang.Class.privateGetDeclaredMethods java.lang.Class.getMethodsRecursive java.lang.Class.getMethodsRecursive java.lang.Class.getMethod0 java.lang.Class.getMethod]
// getFields-super: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredFields0(N) java.lang.Class.privateGetDeclaredFields java.lang.Class.privateGetPublicFields java.lang.Class.privateGetPublicFields java.lang.Class.getFields]
// getField-super: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredFields0(N) java.lang.Class.privateGetDeclaredFields java.lang.Class.getField0 java.lang.Class.getField0 java.lang.Class.getField]
// getMethods-iface: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredMethods0(N) java.lang.Class.privateGetDeclaredMethods java.lang.Class.privateGetPublicMethods java.lang.Class.privateGetPublicMethods java.lang.Class.getMethods]
// getMethod-iface: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredMethods0(N) java.lang.Class.privateGetDeclaredMethods java.lang.Class.getMethodsRecursive java.lang.Class.getMethodsRecursive java.lang.Class.getMethod0 java.lang.Class.getMethod]
// getFields-iface: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredFields0(N) java.lang.Class.privateGetDeclaredFields java.lang.Class.privateGetPublicFields java.lang.Class.privateGetPublicFields java.lang.Class.getFields]
// getField-iface: java.lang.NoClassDefFoundError [java.lang.Class.getDeclaredFields0(N) java.lang.Class.privateGetDeclaredFields java.lang.Class.getField0 java.lang.Class.getField0 java.lang.Class.getField]
// HOTSPOT_EXPECTED_END
//
// CratonVM before 2026-10-07 (with `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES=1`,
// then opt-in): `--jdk-only` listed no frame for the three `getFields` rows,
// `--compatible` none for any row and printed `ok` for five; since then
// (the switch on by default) HotSpot's lines in both modes.
//
// SETUP: none; `java|cratonvm [--compatible] -cp . I47ListingSignatureTypeFrames`.
import java.io.InputStream;

public class I47ListingSignatureTypeFrames {
    public static class Gone {}

    public static class Own {
        public Gone f;
        public Own() {}
        public Own(Gone g) {}
        public void m(Gone g) {}
        public void plain() {}
    }

    public static class Super {
        public Gone f;
        public void m(Gone g) {}
    }

    public static class Sub extends Super {
        public int x;
        public Sub() {}
        public void plain() {}
    }

    public interface Iface {
        Gone F = null;

        void m(Gone g);
    }

    public static abstract class Impl implements Iface {
        public int x;
        public Impl() {}
        public void plain() {}
    }

    static final String PREFIX = "I47ListingSignatureTypeFrames$";

    /** Defines the probe's nested classes itself; refuses `Gone`. */
    static final class Refusing extends ClassLoader {
        Refusing(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null && name.startsWith(PREFIX)) {
                    if (name.equals(PREFIX + "Gone")) {
                        throw new ClassNotFoundException(name);
                    }
                    try (InputStream in = getParent().getResourceAsStream(name + ".class")) {
                        byte[] b = in.readAllBytes();
                        c = defineClass(name, b, 0, b.length);
                    } catch (java.io.IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return c != null ? c : super.loadClass(name, false);
            }
        }
    }

    interface Door {
        Object run(Class<?> c) throws Throwable;
    }

    static void row(String name, String holder, Door door) {
        Class<?> c;
        try {
            c = Class.forName(PREFIX + holder, false,
                    new Refusing(I47ListingSignatureTypeFrames.class.getClassLoader()));
        } catch (Throwable t) {
            System.out.println(name + ": setup " + t);
            return;
        }
        try {
            door.run(c);
            System.out.println(name + ": ok");
        } catch (Throwable t) {
            StringBuilder sb = new StringBuilder();
            for (StackTraceElement e : t.getStackTrace()) {
                if (e.getClassName().equals("I47ListingSignatureTypeFrames")
                        || e.getClassName().startsWith(PREFIX)) {
                    break;
                }
                if (sb.length() > 0) {
                    sb.append(' ');
                }
                sb.append(e.getClassName()).append('.').append(e.getMethodName());
                if (e.isNativeMethod()) {
                    sb.append("(N)");
                }
            }
            System.out.println(name + ": " + t.getClass().getName() + " [" + sb + "]");
        }
    }

    public static void main(String[] args) {
        row("getDeclaredMethods-own", "Own", c -> c.getDeclaredMethods());
        row("getDeclaredFields-own", "Own", c -> c.getDeclaredFields());
        row("getDeclaredConstructors-own", "Own", c -> c.getDeclaredConstructors());
        row("getDeclaredMethod-own", "Own", c -> c.getDeclaredMethod("plain"));
        row("getDeclaredField-own", "Own", c -> c.getDeclaredField("f"));
        row("getDeclaredConstructor-own", "Own", c -> c.getDeclaredConstructor());
        row("getMethods-own", "Own", c -> c.getMethods());
        row("getMethod-own", "Own", c -> c.getMethod("plain"));
        row("getFields-own", "Own", c -> c.getFields());
        row("getField-own", "Own", c -> c.getField("f"));
        row("getConstructors-own", "Own", c -> c.getConstructors());
        row("getConstructor-own", "Own", c -> c.getConstructor());
        row("getMethods-super", "Sub", c -> c.getMethods());
        row("getMethod-super", "Sub", c -> c.getMethod("absent"));
        row("getFields-super", "Sub", c -> c.getFields());
        row("getField-super", "Sub", c -> c.getField("absent"));
        row("getMethods-iface", "Impl", c -> c.getMethods());
        row("getMethod-iface", "Impl", c -> c.getMethod("absent"));
        row("getFields-iface", "Impl", c -> c.getFields());
        row("getField-iface", "Impl", c -> c.getField("absent"));
    }
}
