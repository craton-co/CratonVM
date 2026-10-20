// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.lang.reflect.ParameterizedType;
import java.lang.reflect.Type;
import java.util.List;
import java.util.Map;

/**
 * SBR wave 3, lane {@code reflect}: measurement 7 micro for the reflective
 * hot paths the lane considered -- {@code Field.getInt/setInt/get/set},
 * {@code Method.invoke}, {@code Constructor.newInstance}, and the generic
 * {@code ParameterizedTypeImpl} accessors. Prints ns/op per row per round;
 * run base and lane binaries INTERLEAVED, JIT and {@code --nojit}.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . SbrW3ReflectBench [rounds] [iters]
 * </pre>
 */
public class SbrW3ReflectBench {
    public static class H {
        public int i = 1;
        public long l = 2;
        public Object o = "x";
        public Map<String, List<Integer>> g;

        public H() {}

        public int add(int a) {
            return a + i;
        }
    }

    static long sink;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;
        H h = new H();
        Field fi = H.class.getField("i");
        Field fl = H.class.getField("l");
        Field fo = H.class.getField("o");
        Method add = H.class.getMethod("add", int.class);
        Constructor<H> ctor = H.class.getConstructor();
        // Tolerates an erased answer (a dial arm without the Field signature
        // fix), so the other rows still time; the pt rows then print -1.
        Type gt = H.class.getField("g").getGenericType();
        ParameterizedType pt = gt instanceof ParameterizedType p ? p : null;
        Object[] one = new Object[] {1};
        for (int r = 0; r < rounds; r++) {
            StringBuilder out = new StringBuilder("round " + r);
            long t = System.nanoTime();
            for (int k = 0; k < n; k++) sink += fi.getInt(h);
            out.append(" getInt=").append((System.nanoTime() - t) / n);
            t = System.nanoTime();
            for (int k = 0; k < n; k++) fi.setInt(h, k);
            out.append(" setInt=").append((System.nanoTime() - t) / n);
            t = System.nanoTime();
            for (int k = 0; k < n; k++) sink += fl.getLong(h);
            out.append(" getLong=").append((System.nanoTime() - t) / n);
            t = System.nanoTime();
            for (int k = 0; k < n; k++) sink += fo.get(h).hashCode();
            out.append(" getObj=").append((System.nanoTime() - t) / n);
            t = System.nanoTime();
            for (int k = 0; k < n; k++) fo.set(h, "y");
            out.append(" setObj=").append((System.nanoTime() - t) / n);
            t = System.nanoTime();
            for (int k = 0; k < n; k++) sink += (Integer) fi.get(h);
            out.append(" getBoxed=").append((System.nanoTime() - t) / n);
            t = System.nanoTime();
            for (int k = 0; k < n; k++) sink += (Integer) add.invoke(h, one);
            out.append(" invoke=").append((System.nanoTime() - t) / n);
            t = System.nanoTime();
            for (int k = 0; k < n / 4; k++) sink += ctor.newInstance().i;
            out.append(" newInst=").append((System.nanoTime() - t) / (n / 4));
            if (pt == null) {
                System.out.println(out.append(" ptArgs=-1 ptToString=-1"));
                continue;
            }
            t = System.nanoTime();
            for (int k = 0; k < n; k++) {
                Type[] a = pt.getActualTypeArguments();
                sink += a.length + (pt.getRawType() == Map.class ? 1 : 0);
            }
            out.append(" ptArgs=").append((System.nanoTime() - t) / n);
            t = System.nanoTime();
            for (int k = 0; k < n / 20; k++) sink += pt.toString().length();
            out.append(" ptToString=").append((System.nanoTime() - t) / (n / 20));
            System.out.println(out);
        }
        System.out.println("sink " + (sink != 0));
    }
}
