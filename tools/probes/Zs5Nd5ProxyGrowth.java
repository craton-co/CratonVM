// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Proxy;

/**
 * zs s5/nd5: does a proxy class defined for a FRESH loader per iteration (the
 * {@code Zs3Nd3UpcallMints proxy} step) stay live after its loader is
 * unreachable? (ZGC round zs, wave s5, lane nd5.)
 *
 * <p>Each round defines {@code per} proxy classes, one per new loader, then
 * calls {@code System.gc()} and reports the heap in use after it. HotSpot unloads
 * the loaders and their proxy classes: the used figure stays flat. A leak shows
 * as a steady rise per round. stdout: one deterministic verdict line
 * ({@code grew=<bool>} against a 4 MiB bar (ZGC reports the heap in use at 2 MiB granularity on HotSpot)); the per-round
 * figures and the CPU time go to stderr.
 *
 * <p>Usage: {@code java -XX:+UseZGC -Xmx64m Zs5Nd5ProxyGrowth [rounds] [per]}
 * (defaults 6 and 500).
 */
public class Zs5Nd5ProxyGrowth {
    @SuppressWarnings("deprecation")
    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        int per = args.length > 1 ? Integer.parseInt(args[1]) : 500;
        Runtime rt = Runtime.getRuntime();
        long first = -1;
        long last = 0;
        int bad = 0;
        long t0 = System.nanoTime();
        for (int r = 0; r <= rounds; r++) {
            if (r > 0) {
                for (int i = 0; i < per; i++) {
                    ClassLoader fresh = new ClassLoader(Zs5Nd5ProxyGrowth.class.getClassLoader()) {};
                    Class<?> c = Proxy.getProxyClass(fresh, Runnable.class);
                    if (!Proxy.isProxyClass(c) || c.getClassLoader() != fresh) {
                        bad++;
                    }
                }
            }
            System.gc();
            long used = rt.totalMemory() - rt.freeMemory();
            if (first < 0) {
                first = used;
            }
            last = used;
            System.err.println("round=" + r + " defines=" + (r * per) + " used_kb=" + used / 1024
                    + " ms=" + (System.nanoTime() - t0) / 1_000_000);
        }
        long defines = (long) rounds * per;
        long perThousand = defines == 0 ? 0 : (last - first) * 1000 / defines;
        System.err.println("growth_bytes_per_1000_defines=" + perThousand);
        System.out.println("defines=" + defines + " bad=" + bad + " grew=" + (last - first > 4L * 1024 * 1024));
    }
}
