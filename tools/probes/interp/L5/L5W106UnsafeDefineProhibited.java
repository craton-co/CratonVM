// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 106, lane L5 -- `jdk.internal.misc.Unsafe.
// defineClass` of a class in a `java.*` package. HotSpot's
// `Unsafe_DefineClass0` -> `JVM_DefineClass` runs the same
// `check_prohibited_package` as `ClassLoader.defineClass1`: every non-null
// loader but the platform loader is refused with `SecurityException: Class
// loader (instance of): <nameAndId> tried to load prohibited package name:
// java.l5w106`. CratonVM printed its own text for a user loader (`Prohibited
// package name: java.l5w106.Spoof (privileged define into loader
// user-defined(<n>) ...)`) and DEFINED the class in the application loader
// (its privileged-define exemption cannot tell an explicit application loader
// from a JDK bridge's null loader). The platform loader row is the control:
// HotSpot exempts it. `@<hash>` is masked to `@H`.
//
// Run (no setup; reflection, so javac needs no flag):
//   javac -d out L5W106UnsafeDefineProhibited.java
//   java --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L5W106UnsafeDefineProhibited
//   cratonvm --jdk-only   [--nojit] --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L5W106UnsafeDefineProhibited
//   cratonvm --compatible [--nojit] --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L5W106UnsafeDefineProhibited
// Without the export every row prints `unavailable <exception>`.
//
// HOTSPOT_EXPECTED derived from the JDK 25 sources (the text measured for
// `defineClass1` by L5W42DefineClassErrors), not run.
// HOTSPOT_EXPECTED_BEGIN
// user-named=java.lang.SecurityException: Class loader (instance of): 'o' @H tried to load prohibited package name: java.l5w106
// user-null-name=java.lang.SecurityException: Class loader (instance of): 'o' @H tried to load prohibited package name: java.l5w106
// user-unnamed=java.lang.SecurityException: Class loader (instance of): L5W106UnsafeDefineProhibited$Open @H tried to load prohibited package name: java.l5w106
// app=java.lang.SecurityException: Class loader (instance of): 'app' tried to load prohibited package name: java.l5w106
// platform=defined java.l5w106.Spoof platform=true
// HOTSPOT_EXPECTED_END

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;

public class L5W106UnsafeDefineProhibited {
    static final String NAME = "java/l5w106/Spoof";

    static class Open extends ClassLoader {
        Open() {
            super(null);
        }

        Open(String name) {
            super(name, null);
        }
    }

    static byte[] classBytes() {
        int[] head = {0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52, 0, 5};
        byte[] out = new byte[128];
        int n = 0;
        for (int v : head) out[n++] = (byte) v;
        n = classEntry(out, n, 2);
        n = utf8(out, n, NAME);
        n = classEntry(out, n, 4);
        n = utf8(out, n, "java/lang/Object");
        int[] tail = {0x00, 0x21, 0, 1, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0};
        for (int v : tail) out[n++] = (byte) v;
        byte[] cls = new byte[n];
        System.arraycopy(out, 0, cls, 0, n);
        return cls;
    }

    static int classEntry(byte[] out, int n, int nameIndex) {
        out[n++] = 7;
        out[n++] = (byte) (nameIndex >> 8);
        out[n++] = (byte) nameIndex;
        return n;
    }

    static int utf8(byte[] out, int n, String s) {
        out[n++] = 1;
        out[n++] = (byte) (s.length() >> 8);
        out[n++] = (byte) s.length();
        for (int i = 0; i < s.length(); i++) out[n++] = (byte) s.charAt(i);
        return n;
    }

    static Object unsafe;
    static Method define;
    static Throwable unavailable;

    static void row(String label, String name, ClassLoader loader) {
        String v;
        if (define == null) {
            v = "unavailable " + unavailable.getClass().getName();
        } else {
            try {
                byte[] b = classBytes();
                Class<?> c = (Class<?>) define.invoke(unsafe, name, b, 0, b.length, loader, null);
                v = "defined " + c.getName() + " platform="
                        + (c.getClassLoader() == ClassLoader.getPlatformClassLoader());
            } catch (Throwable t) {
                Throwable e = t;
                if (e instanceof InvocationTargetException && e.getCause() != null) {
                    e = e.getCause();
                }
                v = e.getClass().getName() + ": "
                        + String.valueOf(e.getMessage()).replaceAll("@[0-9a-f]+", "@H");
            }
        }
        System.out.println(label + "=" + v);
    }

    public static void main(String[] args) {
        try {
            Class<?> u = Class.forName("jdk.internal.misc.Unsafe");
            unsafe = u.getMethod("getUnsafe").invoke(null);
            define = u.getMethod("defineClass", String.class, byte[].class, int.class,
                    int.class, ClassLoader.class, ProtectionDomain.class);
        } catch (Throwable t) {
            unavailable = t;
        }
        row("user-named", "java.l5w106.Spoof", new Open("o"));
        row("user-null-name", null, new Open("o"));
        row("user-unnamed", "java.l5w106.Spoof", new Open());
        row("app", "java.l5w106.Spoof", ClassLoader.getSystemClassLoader());
        row("platform", "java.l5w106.Spoof", ClassLoader.getPlatformClassLoader());
    }
}
