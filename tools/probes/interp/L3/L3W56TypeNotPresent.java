// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L3 (review) --
// docs/internal/fixed-bugs/interpreter-L3-compatible-generic-reflection-erases-a-missing-type-FIXED-20261021.md:
// a generic signature naming a class its holder's loader cannot find. HotSpot
// raises `TypeNotPresentException` (cause `ClassNotFoundException`) when the
// type is reified -- `getGenericType()` at once, a type variable's bound only
// at `getBounds()` (the variable itself is built). `--compatible`'s natives
// erase the type to `Object` and leave a bound `null`.
//
// The holder is defined by a loader that sees only the holder's own bytes
// (parent: the platform loader), so `Gone` is missing for it alone.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// field: java.lang.TypeNotPresentException Type L3W56TypeNotPresent$Gone not present cause=java.lang.ClassNotFoundException: L3W56TypeNotPresent$Gone
// field-arg: java.lang.TypeNotPresentException Type L3W56TypeNotPresent$Gone not present cause=java.lang.ClassNotFoundException: L3W56TypeNotPresent$Gone
// wild: java.lang.TypeNotPresentException Type L3W56TypeNotPresent$Gone not present cause=java.lang.ClassNotFoundException: L3W56TypeNotPresent$Gone
// tv: T
// tv-bounds: java.lang.TypeNotPresentException Type L3W56TypeNotPresent$Gone not present cause=java.lang.ClassNotFoundException: L3W56TypeNotPresent$Gone
// ret: java.util.List<T>
// field-again: java.lang.TypeNotPresentException Type L3W56TypeNotPresent$Gone not present cause=java.lang.ClassNotFoundException: L3W56TypeNotPresent$Gone
// HOTSPOT_EXPECTED_END
//
// CratonVM (wave 56, `--compatible`): the natives resolve the name past the
// holder's loader (`generics::resolve_class_id_in_generic_scope`'s
// loader-blind fallbacks) and print `field: java.util.List<L3W56TypeNotPresent$Gone>`,
// `field-arg: class L3W56TypeNotPresent$Gone`, `tv-bounds: [class L3W56TypeNotPresent$Gone]`;
// with the class file deleted instead they erase it (`List<java.lang.Object>`,
// `? extends ?`, `[null]`). `--jdk-only` matches HotSpot.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W56TypeNotPresent`.
import java.io.InputStream;
import java.lang.reflect.Method;
import java.lang.reflect.ParameterizedType;
import java.util.Arrays;
import java.util.List;
import java.util.Map;

public class L3W56TypeNotPresent {
    public static class Gone {
    }

    public static class Holder {
        public List<Gone> gone;
        public Map<String, ? extends Gone> wild;

        public static <T extends Gone> List<T> m(Object t) {
            return null;
        }
    }

    /** Sees only `Holder`'s bytes; everything else from the platform loader. */
    static final class Isolating extends ClassLoader {
        Isolating() {
            super(ClassLoader.getPlatformClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals("L3W56TypeNotPresent$Holder")) {
                throw new ClassNotFoundException(name);
            }
            try (InputStream in = L3W56TypeNotPresent.class.getResourceAsStream("L3W56TypeNotPresent$Holder.class")) {
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length);
            } catch (java.io.IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }
    }

    interface Call {
        Object call() throws Exception;
    }

    static void row(String n, Call c) {
        try {
            System.out.println(n + ": " + c.call());
        } catch (Throwable t) {
            System.out.println(n + ": " + t.getClass().getName() + " " + t.getMessage() + " cause=" + t.getCause());
        }
    }

    public static void main(String[] a) throws Exception {
        Class<?> h = new Isolating().loadClass("L3W56TypeNotPresent$Holder");
        row("field", () -> h.getField("gone").getGenericType());
        row("field-arg", () -> ((ParameterizedType) h.getField("gone").getGenericType()).getActualTypeArguments()[0]);
        row("wild", () -> h.getField("wild").getGenericType());
        Method m = null;
        for (Method x : h.getDeclaredMethods()) {
            if (x.getName().equals("m")) {
                m = x;
            }
        }
        Method mm = m;
        row("tv", () -> mm.getTypeParameters()[0]);
        row("tv-bounds", () -> Arrays.toString(mm.getTypeParameters()[0].getBounds()));
        row("ret", () -> mm.getGenericReturnType());
        row("field-again", () -> h.getField("gone").getGenericType());
    }
}
