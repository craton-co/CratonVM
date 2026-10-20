// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.lang.reflect.Proxy;

/**
 * zs s5/nd5: which step of the {@code Zs3Nd3UpcallMints proxy} iteration keeps
 * its fresh loader alive (ZGC round zs, wave s5, lane nd5).
 *
 * <ul>
 *   <li>{@code plain}: a fresh loader that defines nothing;
 *   <li>{@code define}: a fresh loader that defines one tiny class
 *       ({@code ClassLoader.defineClass});
 *   <li>{@code ctor}: {@code define}, then {@code getConstructor()} of the
 *       class (the reflection {@code Proxy.getProxyClass} does);
 *   <li>{@code proxy}: {@code Proxy.getProxyClass(fresh, Runnable.class)} (a
 *       dynamic module per loader, then a proxy class defined in it).
 * </ul>
 *
 * <p>Per round: {@code per} iterations, {@code System.gc()}, the heap in use.
 * stdout: one deterministic verdict line ({@code grew=<bool>} against a
 * 4 MiB bar (ZGC reports the heap in use at 2 MiB granularity on HotSpot)); figures on stderr. HotSpot: {@code grew=false} for all
 * three. Usage: {@code java -XX:+UseZGC -Xmx64m Zs5Nd5LoaderGrowth <mode>
 * [rounds] [per]} (defaults 3 and 400).
 */
public class Zs5Nd5LoaderGrowth {
    static volatile Object sink;

    /** The class each {@code define} loader defines (bytes from the class path). */
    public static final class Tiny {
        public static int answer() {
            return 42;
        }
    }

    static final class Definer extends ClassLoader {
        Definer(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(byte[] b) {
            return defineClass("Zs5Nd5LoaderGrowth$Tiny", b, 0, b.length);
        }
    }

    @SuppressWarnings("deprecation")
    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "plain";
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        int per = args.length > 2 ? Integer.parseInt(args[2]) : 400;
        byte[] tiny;
        try (InputStream in = Zs5Nd5LoaderGrowth.class.getResourceAsStream("Zs5Nd5LoaderGrowth$Tiny.class")) {
            tiny = in.readAllBytes();
        }
        ClassLoader app = Zs5Nd5LoaderGrowth.class.getClassLoader();
        Runtime rt = Runtime.getRuntime();
        long first = -1;
        long last = 0;
        int bad = 0;
        long t0 = System.nanoTime();
        for (int r = 0; r <= rounds; r++) {
            for (int i = 0; r > 0 && i < per; i++) {
                switch (mode) {
                    case "plain": {
                        ClassLoader fresh = new ClassLoader(app) {};
                        sink = fresh;
                        break;
                    }
                    case "define": {
                        Class<?> c = new Definer(app).define(tiny);
                        if (c.getClassLoader() == app) {
                            bad++;
                        }
                        break;
                    }
                    case "ctor": {
                        Class<?> c = new Definer(app).define(tiny);
                        if (c.getConstructor().getDeclaringClass() != c) {
                            bad++;
                        }
                        break;
                    }
                    case "proxy": {
                        ClassLoader fresh = new ClassLoader(app) {};
                        Class<?> c = Proxy.getProxyClass(fresh, Runnable.class);
                        if (c.getClassLoader() != fresh) {
                            bad++;
                        }
                        break;
                    }
                    default:
                        throw new IllegalArgumentException(mode);
                }
            }
            sink = null;
            System.gc();
            long used = rt.totalMemory() - rt.freeMemory();
            if (first < 0) {
                first = used;
            }
            last = used;
            System.err.println("mode=" + mode + " round=" + r + " used_kb=" + used / 1024
                    + " ms=" + (System.nanoTime() - t0) / 1_000_000);
        }
        long n = (long) rounds * per;
        long perThousand = n == 0 ? 0 : (last - first) * 1000 / n;
        System.err.println("growth_bytes_per_1000=" + perThousand);
        System.out.println("mode=" + mode + " n=" + n + " bad=" + bad + " grew=" + (last - first > 4L * 1024 * 1024));
    }
}
