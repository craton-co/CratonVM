// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * gcf f1/j (2026-10-01): JNI functions whose answers differed from HotSpot 25,
 * one stdout line each, all deterministic (HotSpot 25 {@code -XX:+UseSerialGC}
 * is the oracle):
 * <ul>
 *   <li>{@code version}: {@code GetVersion} -- HotSpot 25 answers
 *       {@code JNI_VERSION_24} ({@code 0x180000});</li>
 *   <li>{@code module-*}: {@code GetModule} -- the class's {@code Module},
 *       the very object {@code Class.getModule()} answers;</li>
 *   <li>{@code utf-*}: {@code NewStringUTF(GetStringUTFChars(s))} -- the
 *       same string for a NUL and a supplementary character, and for a
 *       70 000-character string;</li>
 *   <li>{@code to-reflected-*}: {@code ToReflectedMethod} /
 *       {@code ToReflectedField} -- a usable {@code Method} / {@code Field};</li>
 *   <li>{@code from-reflected-*}: {@code FromReflectedMethod} /
 *       {@code FromReflectedField} of a real reflection object -- an id that
 *       calls / reads the member;</li>
 *   <li>{@code getenv-detached}: {@code GetEnv} on a pthread that never
 *       attached -- {@code JNI_EDETACHED} ({@code -2}) with a NULL env.</li>
 * </ul>
 * HotSpot prints:
 * <pre>
 *   version: 0x180000
 *   module-string: true
 *   module-probe: true
 *   utf-roundtrip: true
 *   utf-long: 70000
 *   getenv-detached: -2
 *   to-reflected-method: target/1/GcfF1jJniSemanticsProbe
 *   to-reflected-field: answer/7
 *   from-reflected-method: 42
 *   from-reflected-field: 7
 * </pre>
 *
 * <p>Commands (Linux):
 * <pre>
 *   gcc -O1 -shared -fPIC -pthread -I"$JDK/include" -I"$JDK/include/linux" \
 *       -o /tmp/libgcff1jjnisem.so tools/probes/jni/GcfF1jJniSemanticsProbe.c
 *   javac -d /tmp/gcff1jjnisem tools/bench/GcfF1jJniSemanticsProbe.java
 *   java -XX:+UseSerialGC -cp /tmp/gcff1jjnisem GcfF1jJniSemanticsProbe /tmp/libgcff1jjnisem.so
 * </pre>
 */
public final class GcfF1jJniSemanticsProbe {

    static int answer = 7;

    static int target(int x) {
        return x * 3;
    }

    static native int jniVersion();

    static native Object module(Class<?> c);

    static native String utfRoundTrip(String s);

    static native Object reflectedMethod(Class<?> c);

    static native int callViaFromReflected(Method m, int x);

    static native Object reflectedField(Class<?> c);

    static native int readViaFromReflected(Field f);

    static native int getEnvOnFreshThread();

    interface Check {
        String run() throws Throwable;
    }

    static void line(String name, Check c) {
        String v;
        try {
            v = c.run();
        } catch (Throwable t) {
            v = "threw " + t.getClass().getName();
        }
        System.out.println(name + ": " + v);
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: GcfF1jJniSemanticsProbe /abs/path/libgcff1jjnisem.so");
            System.exit(2);
        }
        System.load(args[0]);
        Class<?> me = GcfF1jJniSemanticsProbe.class;
        line("version", () -> "0x" + Integer.toHexString(jniVersion()));
        line("module-string", () -> String.valueOf(module(String.class) == String.class.getModule()));
        line("module-probe", () -> String.valueOf(module(me) == me.getModule()));
        String odd = "a\u0000b😀";
        line("utf-roundtrip", () -> String.valueOf(odd.equals(utfRoundTrip(odd))));
        String big = "y".repeat(70_000);
        line("utf-long", () -> {
            String back = utfRoundTrip(big);
            return back == null ? "null" : String.valueOf(back.length());
        });
        // Before the reflection lines: a wrong member id there can fault.
        line("getenv-detached", () -> String.valueOf(getEnvOnFreshThread()));
        line("to-reflected-method", () -> {
            Object r = reflectedMethod(me);
            if (!(r instanceof Method m)) {
                return "not a Method: " + (r == null ? "null" : r.getClass().getName());
            }
            return m.getName() + "/" + m.getParameterCount() + "/"
                    + m.getDeclaringClass().getSimpleName();
        });
        line("to-reflected-field", () -> {
            Object r = reflectedField(me);
            if (!(r instanceof Field f)) {
                return "not a Field: " + (r == null ? "null" : r.getClass().getName());
            }
            return f.getName() + "/" + f.getInt(null);
        });
        line("from-reflected-method",
                () -> String.valueOf(callViaFromReflected(me.getDeclaredMethod("target", int.class), 14)));
        line("from-reflected-field",
                () -> String.valueOf(readViaFromReflected(me.getDeclaredField("answer"))));
    }
}
