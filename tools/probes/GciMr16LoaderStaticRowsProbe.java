// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.List;

/**
 * gci i16/mr (2026-10-14): user-loader statics as the ONLY edge to their
 * object graphs, across allocation-driven G1 concurrent cycles. Page:
 * {@code docs/known-issues/gc/gci-i15vh-g1-marking-scan-rows-filed-while-the-marker-runs-20261014.md}.
 *
 * <p>{@code L} child-first loaders each define {@link Holder}; each class's
 * static {@code graph} holds a fresh {@code Object[]} of {@code int[]} rows,
 * reachable from nothing else (on CratonVM: a {@code metadata_pin} row owned
 * by the loader, filed by every G1 marking scan while the concurrent marker
 * runs). A retained old fill keeps the heap above the IHOP, young churn drives
 * the pauses, and each round replaces one loader's graph and checks every
 * graph's contents. No {@code System.gc()}.
 *
 * <p>Args: {@code [L rounds fillMb]} (defaults 8 60 24). Stdout (HotSpot, any
 * collector): {@code mr16 statics loaders=L rounds=R bad=0}. Diagnostics on
 * stderr.
 */
public class GciMr16LoaderStaticRowsProbe {
    public static class Holder {
        public static Object graph;
    }

    static final String HOLDER = "GciMr16LoaderStaticRowsProbe$Holder";
    static final int ROWS = 2000;
    static final int WIDTH = 32;
    static volatile Object sink;

    static final class ChildFirst extends ClassLoader {
        private final byte[] bytes;

        ChildFirst(byte[] bytes) {
            super(GciMr16LoaderStaticRowsProbe.class.getClassLoader());
            this.bytes = bytes;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(HOLDER)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    c = defineClass(name, bytes, 0, bytes.length);
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }
    }

    static byte[] holderBytes() throws Exception {
        try (InputStream in =
                GciMr16LoaderStaticRowsProbe.class.getResourceAsStream(HOLDER + ".class")) {
            if (in == null) {
                throw new IllegalStateException("no class bytes for " + HOLDER);
            }
            return in.readAllBytes();
        }
    }

    static Object[] build(int seed) {
        Object[] g = new Object[ROWS];
        for (int i = 0; i < ROWS; i++) {
            int[] a = new int[WIDTH];
            for (int k = 0; k < WIDTH; k++) {
                a[k] = seed * 31 + i + k;
            }
            g[i] = a;
        }
        return g;
    }

    static boolean check(Object o, int seed) {
        if (!(o instanceof Object[])) {
            return false;
        }
        Object[] g = (Object[]) o;
        if (g.length != ROWS) {
            return false;
        }
        for (int i = 0; i < ROWS; i++) {
            if (!(g[i] instanceof int[])) {
                return false;
            }
            int[] a = (int[]) g[i];
            if (a.length != WIDTH) {
                return false;
            }
            for (int k = 0; k < WIDTH; k++) {
                if (a[k] != seed * 31 + i + k) {
                    return false;
                }
            }
        }
        return true;
    }

    public static void main(String[] args) throws Exception {
        int loaders = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 60;
        int fillMb = args.length > 2 ? Integer.parseInt(args[2]) : 24;
        byte[] bytes = holderBytes();
        List<ClassLoader> keep = new ArrayList<>();
        Field[] fields = new Field[loaders];
        int[] seeds = new int[loaders];
        for (int l = 0; l < loaders; l++) {
            ChildFirst cl = new ChildFirst(bytes);
            Class<?> c = cl.loadClass(HOLDER);
            if (c.getClassLoader() != cl) {
                throw new IllegalStateException("not child-first: " + c.getClassLoader());
            }
            keep.add(cl);
            fields[l] = c.getField("graph");
            seeds[l] = l + 1;
            fields[l].set(null, build(seeds[l]));
        }
        List<byte[]> fill = new ArrayList<>();
        for (int i = 0; i < fillMb * 16; i++) {
            fill.add(new byte[64 * 1024]);
        }
        int bad = 0;
        long churned = 0;
        long started = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 4000; i++) {
                sink = new byte[256 + (i & 1023)];
                churned++;
            }
            if (!fill.isEmpty()) {
                fill.set(r % fill.size(), new byte[64 * 1024]);
            }
            int l = r % loaders;
            seeds[l] += loaders;
            fields[l].set(null, build(seeds[l]));
            for (int k = 0; k < loaders; k++) {
                if (!check(fields[k].get(null), seeds[k])) {
                    bad++;
                    System.err.println("[mr16] round=" + r + " loader=" + k + " graph damaged");
                }
            }
        }
        System.out.println("mr16 statics loaders=" + loaders + " rounds=" + rounds + " bad=" + bad);
        System.err.println("[mr16] churned=" + churned + " keep=" + keep.size() + " fill=" + fill.size()
                + " ms=" + (System.nanoTime() - started) / 1_000_000);
    }
}
