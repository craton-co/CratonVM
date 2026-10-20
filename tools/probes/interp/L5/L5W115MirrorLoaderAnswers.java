// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L5: the loader and module answers of a
// freshly minted `Class` mirror, which `CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP`
// (default off) computes from one defining-loader lookup and one class-row
// read instead of three lookups and three reads (umbrella
// `i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot`, "Wave 115").
// Every compared line must be the same with the switch off and on, both modes,
// JIT and --nojit. `TIME ` lines are the bench (ignored by the battery).
//
// Run: javac -d . L5W115MirrorLoaderAnswers.java
//      java -cp . L5W115MirrorLoaderAnswers                                  (HotSpot reference)
//      cratonvm --jdk-only -cp . L5W115MirrorLoaderAnswers
//      CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP=1 cratonvm --jdk-only -cp . L5W115MirrorLoaderAnswers
//      (and the same two with --compatible, and each with --nojit)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// defined-loader-is-own true
// defined-module-is-unnamed-of-own true
// defined-module-named false
// defined-package L5W115MirrorLoaderAnswers$Leaf -> []
// array-loader-is-own true
// array2-loader-is-own true
// array-module-is-own true
// lambda-loader-is-own true
// lambda-module-is-own true
// hidden-loader-is-own true
// hidden-module-is-own true
// after-gc-loader-is-own true
// app-class-loader app
// app-array-loader app
// boot-class-loader null
// boot-module java.base
// platform-class-loader platform
// platform-module java.sql
// crowd-loaders-own 200
// crowd-modules-own 200
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Array;
import java.util.function.Supplier;

public class L5W115MirrorLoaderAnswers {
    public static class Leaf implements Supplier<Object> {
        public Object get() {
            Runnable r = () -> { };
            return r;
        }

        public static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    static byte[] bytesOf(String resource) throws Exception {
        try (InputStream in = L5W115MirrorLoaderAnswers.class.getResourceAsStream(resource)) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            in.transferTo(out);
            return out.toByteArray();
        }
    }

    static final class OneShot extends ClassLoader {
        OneShot() {
            super(L5W115MirrorLoaderAnswers.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static String loaderName(ClassLoader l) {
        return l == null ? "null" : String.valueOf(l.getName());
    }

    public static void main(String[] args) throws Throwable {
        byte[] leafBytes = bytesOf("L5W115MirrorLoaderAnswers$Leaf.class");
        String leafName = "L5W115MirrorLoaderAnswers$Leaf";

        OneShot loader = new OneShot();
        Class<?> c = loader.define(leafName, leafBytes);
        System.out.println("defined-loader-is-own " + (c.getClassLoader() == loader));
        System.out.println("defined-module-is-unnamed-of-own " + (c.getModule() == loader.getUnnamedModule()));
        System.out.println("defined-module-named " + c.getModule().isNamed());
        System.out.println("defined-package " + c.getName() + " -> [" + c.getPackageName() + "]");

        Class<?> arr = Array.newInstance(c, 0).getClass();
        Class<?> arr2 = Array.newInstance(c, 0, 0).getClass();
        System.out.println("array-loader-is-own " + (arr.getClassLoader() == loader));
        System.out.println("array2-loader-is-own " + (arr2.getClassLoader() == loader));
        System.out.println("array-module-is-own " + (arr.getModule() == loader.getUnnamedModule()));

        @SuppressWarnings("unchecked")
        Supplier<Object> s = (Supplier<Object>) c.getConstructor().newInstance();
        Class<?> lambda = s.get().getClass();
        System.out.println("lambda-loader-is-own " + (lambda.getClassLoader() == loader));
        System.out.println("lambda-module-is-own " + (lambda.getModule() == loader.getUnnamedModule()));

        MethodHandles.Lookup lk = (MethodHandles.Lookup) c.getMethod("lookup").invoke(null);
        Class<?> hidden = lk.defineHiddenClass(leafBytes, true).lookupClass();
        System.out.println("hidden-loader-is-own " + (hidden.getClassLoader() == loader));
        System.out.println("hidden-module-is-own " + (hidden.getModule() == loader.getUnnamedModule()));

        System.gc();
        System.out.println("after-gc-loader-is-own " + (c.getClassLoader() == loader));

        System.out.println("app-class-loader " + loaderName(L5W115MirrorLoaderAnswers.class.getClassLoader()));
        System.out.println("app-array-loader " + loaderName(L5W115MirrorLoaderAnswers[].class.getClassLoader()));
        System.out.println("boot-class-loader " + loaderName(String.class.getClassLoader()));
        System.out.println("boot-module " + String.class.getModule().getName());
        Class<?> sql = Class.forName("java.sql.Date");
        System.out.println("platform-class-loader " + loaderName(sql.getClassLoader()));
        System.out.println("platform-module " + sql.getModule().getName());

        // The crowd: many loaders, one fresh mirror each (the switch's hot path).
        int rounds = 200;
        int loadersOwn = 0;
        int modulesOwn = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < rounds; i++) {
            OneShot l = new OneShot();
            Class<?> k = l.define(leafName, leafBytes);
            if (k.getClassLoader() == l) {
                loadersOwn++;
            }
            if (k.getModule() == l.getUnnamedModule()) {
                modulesOwn++;
            }
        }
        long t1 = System.nanoTime();
        System.out.println("crowd-loaders-own " + loadersOwn);
        System.out.println("crowd-modules-own " + modulesOwn);
        System.out.println("TIME define-and-ask ns/class " + (t1 - t0) / rounds);
    }
}
