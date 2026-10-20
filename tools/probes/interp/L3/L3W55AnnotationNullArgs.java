// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L3 -- a NULL annotation class handed to
// the annotation lookups of `Class`, `Method` and `Constructor`. JDK 25 opens
// each with `Objects.requireNonNull(annotationClass)` (or reaches one through
// `getAnnotation`), so HotSpot throws a message-less `NullPointerException`;
// `--compatible`'s bridges answered "not present" (`null`, `false`, an empty
// array), indistinguishable from a genuine absence. (`Field`'s were fixed in
// an earlier round; its row is the control.)
//
// Each row prints the throwable's class and message, the frames above the
// probe's first frame as `class.method`, then the first probe frame's class.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   class-*        -- `Class.getAnnotation` / `getDeclaredAnnotation` /
//                     `getAnnotationsByType` / `getDeclaredAnnotationsByType` /
//                     `isAnnotationPresent`;
//   method-*       -- the same on a `Method`;
//   ctor-*         -- `getAnnotation` / `isAnnotationPresent` /
//                     `getAnnotationsByType` on a `Constructor`;
//   field-get      -- `Field.getAnnotation` (control);
//   class-get-again -- `class-get` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// class-get: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getAnnotation] L3W55AnnotationNullArgs
// class-declared: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getDeclaredAnnotation] L3W55AnnotationNullArgs
// class-by-type: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getAnnotationsByType] L3W55AnnotationNullArgs
// class-declared-by-type: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getDeclaredAnnotationsByType] L3W55AnnotationNullArgs
// class-present: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getAnnotation java.lang.reflect.AnnotatedElement.isAnnotationPresent java.lang.Class.isAnnotationPresent] L3W55AnnotationNullArgs
// method-get: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Executable.getAnnotation java.lang.reflect.Method.getAnnotation] L3W55AnnotationNullArgs
// method-declared: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Executable.getAnnotation java.lang.reflect.Method.getAnnotation java.lang.reflect.AccessibleObject.getDeclaredAnnotation] L3W55AnnotationNullArgs
// method-by-type: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Executable.getAnnotationsByType] L3W55AnnotationNullArgs
// method-declared-by-type: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Executable.getAnnotationsByType java.lang.reflect.AccessibleObject.getDeclaredAnnotationsByType] L3W55AnnotationNullArgs
// method-present: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Executable.getAnnotation java.lang.reflect.Method.getAnnotation java.lang.reflect.AnnotatedElement.isAnnotationPresent java.lang.reflect.AccessibleObject.isAnnotationPresent] L3W55AnnotationNullArgs
// ctor-get: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Executable.getAnnotation java.lang.reflect.Constructor.getAnnotation] L3W55AnnotationNullArgs
// ctor-present: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Executable.getAnnotation java.lang.reflect.Constructor.getAnnotation java.lang.reflect.AnnotatedElement.isAnnotationPresent java.lang.reflect.AccessibleObject.isAnnotationPresent] L3W55AnnotationNullArgs
// ctor-by-type: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Executable.getAnnotationsByType] L3W55AnnotationNullArgs
// field-get: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Field.getAnnotation] L3W55AnnotationNullArgs
// class-get-again: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getAnnotation] L3W55AnnotationNullArgs
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W55AnnotationNullArgs`.
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.concurrent.Callable;

public class L3W55AnnotationNullArgs {
    public int f;

    public L3W55AnnotationNullArgs() {}

    public void m() {}

    static void raise(String name, Callable<Object> c) {
        try {
            Object v = c.call();
            System.out.println(name + ": no exception (" + (v instanceof Object[] a ? "array " + a.length : String.valueOf(v)) + ")");
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int i = 0;
            while (i < st.length && !st[i].getClassName().startsWith("L3W55")) {
                jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
                i++;
            }
            String first = i < st.length ? st[i].getClassName() : "-";
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> c = L3W55AnnotationNullArgs.class;
        Method m = c.getMethod("m");
        Constructor<?> k = c.getConstructor();
        Field f = c.getField("f");
        raise("class-get", () -> c.getAnnotation(null));
        raise("class-declared", () -> c.getDeclaredAnnotation(null));
        raise("class-by-type", () -> c.getAnnotationsByType(null));
        raise("class-declared-by-type", () -> c.getDeclaredAnnotationsByType(null));
        raise("class-present", () -> c.isAnnotationPresent(null));
        raise("method-get", () -> m.getAnnotation(null));
        raise("method-declared", () -> m.getDeclaredAnnotation(null));
        raise("method-by-type", () -> m.getAnnotationsByType(null));
        raise("method-declared-by-type", () -> m.getDeclaredAnnotationsByType(null));
        raise("method-present", () -> m.isAnnotationPresent(null));
        raise("ctor-get", () -> k.getAnnotation(null));
        raise("ctor-present", () -> k.isAnnotationPresent(null));
        raise("ctor-by-type", () -> k.getAnnotationsByType(null));
        raise("field-get", () -> f.getAnnotation(null));
        raise("class-get-again", () -> c.getAnnotation(null));
    }
}
