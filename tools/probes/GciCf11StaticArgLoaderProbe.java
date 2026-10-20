/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

import java.io.IOException;
import java.io.InputStream;

/**
 * gci i11/cf (2026-10-13): F1 of
 * {@code gci-i10cr-jni-call-path-review-findings-20261013} -- a by-name STATIC
 * call must not dispatch on the class of its first ARGUMENT when that
 * argument is an instance of ANOTHER loader's copy of the owner's name.
 *
 * <p>{@code F} is loaded twice: by the application loader (the copy this
 * class's {@code F.id(...)} constant-pool entry resolves to, JVMS 5.4.3) and
 * by a child-first loader that defines its own copy from the same bytes.
 * {@code F.id(Object)} is a JNI static native that answers its {@code jclass}'s
 * {@code TAG} (1 for the application copy, 2 for the other one). Every call
 * passes an instance of the OTHER copy; HotSpot runs the application copy's
 * {@code id} on every call, so the sum is {@code calls}. The hot loop reaches
 * compiled code, whose static call of a native whose name two loaders define
 * takes the by-name route ({@code invoke_static_or_native_prepared}) when the
 * caller's loader answers the global table's copy; that route's tail read the
 * argument's class as a receiver and ran the other copy (TAG 2).
 *
 * <p>stdout is deterministic; diagnostics on stderr. Usage:
 * {@code GciCf11StaticArgLoaderProbe /abs/path/gcicf11.dll [calls]}.
 * Native half: {@code tools/probes/jni/GciCf11StaticArgLoaderProbe.c}.
 */
public final class GciCf11StaticArgLoaderProbe {
    static final String F_NAME = "GciCf11StaticArgLoaderProbe$F";

    public static final class F {
        static final int TAG =
                F.class.getClassLoader() == GciCf11StaticArgLoaderProbe.class.getClassLoader() ? 1 : 2;

        public F() {
        }

        static native int id(Object other);
    }

    /** Defines its own copy of {@code F}; delegates everything else. */
    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(F_NAME)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    byte[] bytes;
                    try (InputStream in = getParent().getResourceAsStream(F_NAME + ".class")) {
                        if (in == null) {
                            throw new ClassNotFoundException(name);
                        }
                        bytes = in.readAllBytes();
                    } catch (IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                    c = defineClass(name, bytes, 0, bytes.length);
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }
    }

    static long run(Object other, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += F.id(other);
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("usage: GciCf11StaticArgLoaderProbe /abs/path/gcicf11.dll [calls]");
            return;
        }
        System.load(args[0]);
        final int calls = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;
        ClassLoader loader = new ChildFirst(GciCf11StaticArgLoaderProbe.class.getClassLoader());
        Class<?> otherCopy = Class.forName(F_NAME, true, loader);
        Object other = otherCopy.getDeclaredConstructor().newInstance();
        System.out.println("copies-differ: " + (otherCopy != F.class));
        System.out.println("tag-app: " + F.TAG);
        java.lang.reflect.Field otherTag = otherCopy.getDeclaredField("TAG");
        otherTag.setAccessible(true);
        System.out.println("tag-other: " + otherTag.getInt(null));
        long warm = run(other, calls / 10);
        System.out.println("warm: " + (warm == calls / 10 ? "ok" : "WRONG-COPY"));
        int wrongRounds = 0;
        for (int r = 0; r < 5; r++) {
            long s = run(other, calls);
            if (s != calls) {
                wrongRounds++;
                System.err.println("[gci-cf11-static-arg] round " + r + " sum=" + s + " expected=" + calls);
            }
        }
        System.out.println("rounds-right: " + (5 - wrongRounds) + "/5");
        System.out.println("done");
    }
}
