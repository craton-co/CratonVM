// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 53, lane L3 -- reflective natives given a null
// argument raise HotSpot's `NullPointerException`: its message (none, or the
// JDK's own `requireNonNull` text) and, where the funnel's mark can name them,
// its JDK frames (`stackwalker::served_raise_frames`). The VM's natives said
// `Class.isAssignableFrom: argument is null`, `Field.getAnnotation:
// annotationClass is null`, `ClassLoader.getDefinedPackage: name is null`,
// `annotationClass is null`, and a `TypeVariable`'s
// `getAnnotationsByType(null)` answered an empty array.
//
// Rows marked [frames] print the frames above the probe's first frame as
// `class.method` (`(N)` for a native frame); the others print class and
// message only (an interface call: the served class is not known to the
// capture). The first row is repeated as the last, for a cold-VM gap.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// isAssignableFrom: java.lang.NullPointerException null [java.lang.Class.isAssignableFrom(N)] L3W53ReflectionNullArgs.main
// field-getAnnotation: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.reflect.Field.getAnnotation] L3W53ReflectionNullArgs.main
// getDefinedPackage: java.lang.NullPointerException name cannot be null [java.util.Objects.requireNonNull java.lang.ClassLoader.getDefinedPackage] L3W53ReflectionNullArgs.main
// tv-getAnnotation: java.lang.NullPointerException null
// tv-getDeclaredAnnotation: java.lang.NullPointerException null
// tv-getAnnotationsByType: java.lang.NullPointerException null
// isAssignableFrom-again: java.lang.NullPointerException null [java.lang.Class.isAssignableFrom(N)] L3W53ReflectionNullArgs.main
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W53ReflectionNullArgs`.
import java.lang.reflect.Field;
import java.lang.reflect.TypeVariable;

public class L3W53ReflectionNullArgs<T> {
    public int f;

    static void print(String name, boolean frames, Throwable t) {
        String line = name + ": " + t.getClass().getName() + " " + t.getMessage();
        if (frames) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int i = 0;
            while (i < st.length && !st[i].getClassName().startsWith("L3W53")) {
                jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName())
                        .append(st[i].isNativeMethod() ? "(N)" : "");
                i++;
            }
            String first = i < st.length ? st[i].getClassName() + "." + st[i].getMethodName() : "-";
            line += " [" + jdk + "] " + first;
        }
        System.out.println(line);
    }

    public static void main(String[] args) throws Exception {
        Class<?> c = L3W53ReflectionNullArgs.class;
        Field f = c.getField("f");
        ClassLoader loader = c.getClassLoader();
        TypeVariable<?> tv = c.getTypeParameters()[0];
        try { System.out.println("isAssignableFrom: " + c.isAssignableFrom(null)); } catch (Throwable t) { print("isAssignableFrom", true, t); }
        try { System.out.println("field-getAnnotation: " + f.getAnnotation(null)); } catch (Throwable t) { print("field-getAnnotation", true, t); }
        try { System.out.println("getDefinedPackage: " + loader.getDefinedPackage(null)); } catch (Throwable t) { print("getDefinedPackage", true, t); }
        try { System.out.println("tv-getAnnotation: " + tv.getAnnotation(null)); } catch (Throwable t) { print("tv-getAnnotation", false, t); }
        try { System.out.println("tv-getDeclaredAnnotation: " + tv.getDeclaredAnnotation(null)); } catch (Throwable t) { print("tv-getDeclaredAnnotation", false, t); }
        try { System.out.println("tv-getAnnotationsByType: " + tv.getAnnotationsByType(null).length); } catch (Throwable t) { print("tv-getAnnotationsByType", false, t); }
        try { System.out.println("isAssignableFrom-again: " + c.isAssignableFrom(null)); } catch (Throwable t) { print("isAssignableFrom-again", true, t); }
    }
}
