// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 118, lane L5 -- a class a USER loader defines in a
// package of a JDK module (`jdk.internal.reflect`; only `java.*` is
// prohibited) is a member of its loader's UNNAMED module on HotSpot
// (`i44-L5-proposal-a-classs-module-is-an-identity-every-reader-sees`,
// `i45-L5-proposal-one-module-access-question-by-identity`, "Wave 118"). Every
// reader must agree: `Class.getModule()`, the reflective export / open gates
// (`access_control::reflective_module_access`, by identity under
// `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION`) and linkage. CratonVM labels
// the class by its package name (`java.base`); `access_control::member_module_of`
// corrects that for linkage only.
//
// Run: javac -d out L5W118SpoofedPackageModule.java
//      java -cp out L5W118SpoofedPackageModule                         (HotSpot)
//      cratonvm --jdk-only -cp out L5W118SpoofedPackageModule
//      CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION=1 cratonvm --jdk-only -cp out L5W118SpoofedPackageModule
//      (and both with --compatible)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// spoof-module-named false
// spoof-module-is-loaders-unnamed true
// spoof-package jdk.internal.reflect
// reflect-invoke java.lang.IllegalAccessException
// reflect-access-msg class jdk.internal.reflect.L5W118Spoof cannot access class jdk.internal.misc.VM (in module java.base) because module java.base does not export jdk.internal.misc to unnamed module @H
// set-accessible java.lang.reflect.InaccessibleObjectException
// linkage java.lang.IllegalAccessError
// HOTSPOT_EXPECTED_END

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.function.Supplier;

public class L5W118SpoofedPackageModule {
    static final String NESTED = "L5W118SpoofedPackageModule$Spoof";
    static final String SPOOF = "jdk/internal/reflect/L5W118Spoof";
    static final String SPOOF_DOTTED = "jdk.internal.reflect.L5W118Spoof";
    static final String STUB = "L5W118SpoofedPackageModule$VmStub";

    /** `bytes` with every CONSTANT_Utf8 equal to `from` replaced by `to`. */
    static byte[] renameUtf8(byte[] bytes, String from, String to) {
        byte[] f = from.getBytes(StandardCharsets.UTF_8);
        byte[] t = to.getBytes(StandardCharsets.UTF_8);
        java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
        int i = 0;
        while (i < bytes.length) {
            boolean hit = i + 3 + f.length <= bytes.length
                    && bytes[i] == 1
                    && ((bytes[i + 1] & 0xff) << 8 | (bytes[i + 2] & 0xff)) == f.length
                    && java.util.Arrays.equals(bytes, i + 3, i + 3 + f.length, f, 0, f.length);
            if (hit) {
                out.write(1);
                out.write(t.length >> 8);
                out.write(t.length & 0xff);
                out.write(t, 0, t.length);
                i += 3 + f.length;
            } else {
                out.write(bytes[i]);
                i++;
            }
        }
        return out.toByteArray();
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L5W118SpoofedPackageModule.class.getClassLoader());
        }

        Class<?> spoof() throws IOException {
            byte[] b;
            try (InputStream in = getParent().getResourceAsStream(NESTED + ".class")) {
                b = in.readAllBytes();
            }
            b = renameUtf8(b, NESTED, SPOOF);
            b = renameUtf8(b, STUB, "jdk/internal/misc/VM");
            return defineClass(SPOOF_DOTTED, b, 0, b.length);
        }
    }

    public static void main(String[] args) throws Exception {
        Loader l = new Loader();
        Class<?> spoof = l.spoof();
        System.out.println("spoof-module-named " + spoof.getModule().isNamed());
        System.out.println("spoof-module-is-loaders-unnamed " + (spoof.getModule() == l.getUnnamedModule()));
        System.out.println("spoof-package " + spoof.getPackageName());
        @SuppressWarnings("unchecked")
        Supplier<String> s = (Supplier<String>) spoof.getConstructor().newInstance();
        for (String line : s.get().split("\n")) {
            System.out.println(line.replaceAll("@[0-9a-f]+", "@H"));
        }
    }

    /** Stands for `jdk.internal.misc.VM` in the source (renamed in the spoof's bytes). */
    public static class VmStub {
        public static boolean isBooted() {
            return true;
        }
    }

    /** Renamed to `jdk.internal.reflect.L5W118Spoof` and defined by `Loader`. */
    public static class Spoof implements Supplier<String> {
        public String get() {
            StringBuilder sb = new StringBuilder();
            try {
                Class<?> vm = Class.forName("jdk.internal.misc.VM");
                try {
                    vm.getMethod("isBooted").invoke(null);
                    sb.append("reflect-invoke ok\n");
                } catch (IllegalAccessException e) {
                    sb.append("reflect-invoke ").append(e.getClass().getName()).append('\n');
                    sb.append("reflect-access-msg ").append(e.getMessage()).append('\n');
                }
                try {
                    vm.getDeclaredMethod("isBooted").setAccessible(true);
                    sb.append("set-accessible ok\n");
                } catch (RuntimeException e) {
                    sb.append("set-accessible ").append(e.getClass().getName()).append('\n');
                }
                try {
                    // `VmStub` is renamed to `jdk/internal/misc/VM` in these bytes:
                    // an `invokestatic` of the real class, from the spoof.
                    boolean booted = VmStub.isBooted();
                    sb.append("linkage ").append(booted);
                } catch (IllegalAccessError e) {
                    sb.append("linkage ").append(e.getClass().getName());
                }
            } catch (Exception e) {
                sb.append("error ").append(e);
            }
            return sb.toString();
        }
    }
}
